// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    counters,
    counters::{
        PARALLEL_EXECUTION_SECONDS, RAYON_EXECUTION_SECONDS, TASK_EXECUTE_SECONDS,
        TASK_VALIDATE_SECONDS, VM_INIT_SECONDS, WORK_WITH_TASK_SECONDS,
    },
    errors::*,
    executor_utilities::*,
    explicit_sync_wrapper::ExplicitSyncWrapper,
    limit_processor::BlockGasLimitProcessor,
    scheduler::{DependencyStatus, ExecutionTaskType, Scheduler, SchedulerTask, Wave},
    task::{ExecutionStatus, ExecutorTask, TransactionOutput},
    txn_commit_hook::TransactionCommitHook,
    txn_last_input_output::{KeyKind, TxnLastInputOutput},
    types::ReadWriteSummary,
    view::{LatestView, ParallelState, SequentialState, ViewState},
};
use aptos_aggregator::{
    delayed_change::{ApplyBase, DelayedChange},
    delta_change_set::serialize,
    types::{code_invariant_error, expect_ok, PanicOr},
};
use aptos_drop_helper::DEFAULT_DROPPER;
use aptos_logger::{debug, error, info};
use aptos_mvhashmap::{
    types::{Incarnation, MVDelayedFieldsError, TxnIndex, ValueWithLayout},
    unsync_map::UnsyncMap,
    versioned_delayed_fields::CommitError,
    MVHashMap,
};
use aptos_types::{
    block_executor::config::BlockExecutorConfig,
    delayed_fields::PanicError,
    executable::Executable,
    on_chain_config::BlockGasLimitType,
    state_store::{state_value::StateValue, TStateView},
    transaction::{BlockExecutableTransaction as Transaction, BlockOutput},
    write_set::{TransactionWrite, WriteOp},
};
use aptos_vm_logging::{alert, clear_speculative_txn_logs, init_speculative_logs, prelude::*};
use aptos_vm_types::change_set::randomly_check_layout_matches;
use bytes::Bytes;
use claims::assert_none;
use core::panic;
use fail::fail_point;
use move_core_types::{value::MoveTypeLayout, vm_status::StatusCode};
use num_cpus;
use rayon::ThreadPool;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    marker::{PhantomData, Sync},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
};

pub struct BlockExecutor<T, E, S, L, X> {
    // Number of active concurrent tasks, corresponding to the maximum number of rayon
    // threads that may be concurrently participating in parallel execution.
    config: BlockExecutorConfig,
    executor_thread_pool: Arc<ThreadPool>,
    transaction_commit_hook: Option<L>,
    phantom: PhantomData<(T, E, S, L, X)>,
}

impl<T, E, S, L, X> BlockExecutor<T, E, S, L, X>
where
    T: Transaction,
    E: ExecutorTask<Txn = T>,
    S: TStateView<Key = T::Key> + Sync,
    L: TransactionCommitHook<Output = E::Output>,
    X: Executable + 'static,
{
    /// The caller needs to ensure that concurrency_level > 1 (0 is illegal and 1 should
    /// be handled by sequential execution) and that concurrency_level <= num_cpus.
    pub fn new(
        config: BlockExecutorConfig,
        executor_thread_pool: Arc<ThreadPool>,
        transaction_commit_hook: Option<L>,
    ) -> Self {
        assert!(
            config.local.concurrency_level > 0 && config.local.concurrency_level <= num_cpus::get(),
            "Parallel execution concurrency level {} should be between 1 and number of CPUs",
            config.local.concurrency_level
        );
        Self {
            config,
            executor_thread_pool,
            transaction_commit_hook,
            phantom: PhantomData,
        }
    }

    fn execute(
        idx_to_execute: TxnIndex,
        incarnation: Incarnation,
        signature_verified_block: &[T],
        last_input_output: &TxnLastInputOutput<T, E::Output, E::Error>,
        versioned_cache: &MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        executor: &E,
        base_view: &S,
        latest_view: ParallelState<T, X>,
    ) -> Result<bool, PanicOr<ParallelBlockExecutionError>> {
        let _timer = TASK_EXECUTE_SECONDS.start_timer();
        let txn = &signature_verified_block[idx_to_execute as usize];

        // VM execution.
        let sync_view = LatestView::new(base_view, ViewState::Sync(latest_view), idx_to_execute);
        let execute_result = executor.execute_transaction(&sync_view, txn, idx_to_execute);

        let mut prev_modified_keys = last_input_output
            .modified_keys(idx_to_execute)
            .map_or(HashMap::new(), |keys| keys.collect());

        let mut prev_modified_delayed_fields = last_input_output
            .delayed_field_keys(idx_to_execute)
            .map_or(HashSet::new(), |keys| keys.collect());

        let mut read_set = sync_view.take_parallel_reads();

        // For tracking whether the recent execution wrote outside of the previous write/delta set.
        let mut updates_outside = false;
        let mut apply_updates = |output: &E::Output| -> Result<
            Vec<(T::Key, Arc<T::Value>, Option<Arc<MoveTypeLayout>>)>, // Cached resource writes
            PanicError,
        > {
            for (group_key, group_metadata_op, group_ops) in
                output.resource_group_write_set().into_iter()
            {
                if prev_modified_keys.remove(&group_key).is_none() {
                    // Previously no write to the group at all.
                    updates_outside = true;
                }

                versioned_cache.data().write(
                    group_key.clone(),
                    idx_to_execute,
                    incarnation,
                    // Group metadata op needs no layout (individual resources in groups do).
                    Arc::new(group_metadata_op),
                    None,
                );
                if versioned_cache.group_data().write(
                    group_key,
                    idx_to_execute,
                    incarnation,
                    group_ops.into_iter(),
                ) {
                    // Should return true if writes outside.
                    updates_outside = true;
                }
            }

            let resource_write_set = output.resource_write_set();

            // Then, process resource & aggregator_v1 & module writes.
            for (k, v, maybe_layout) in resource_write_set.clone().into_iter().chain(
                output
                    .aggregator_v1_write_set()
                    .into_iter()
                    .map(|(state_key, write_op)| (state_key, Arc::new(write_op), None)),
            ) {
                if prev_modified_keys.remove(&k).is_none() {
                    updates_outside = true;
                }
                versioned_cache
                    .data()
                    .write(k, idx_to_execute, incarnation, v, maybe_layout);
            }

            for (k, v) in output.module_write_set().into_iter() {
                if prev_modified_keys.remove(&k).is_none() {
                    updates_outside = true;
                }
                versioned_cache.modules().write(k, idx_to_execute, v);
            }

            // Then, apply deltas.
            for (k, d) in output.aggregator_v1_delta_set().into_iter() {
                if prev_modified_keys.remove(&k).is_none() {
                    updates_outside = true;
                }
                versioned_cache.data().add_delta(k, idx_to_execute, d);
            }

            let delayed_field_change_set = output.delayed_field_change_set();

            // TODO[agg_v2](optimize): see if/how we want to incorporate DeltaHistory from read set into versoined_delayed_fields.
            // Without that, currently materialized reads cannot check history and fail early.
            //
            // We can extract histories with something like the code below,
            // and then change change.into_entry_no_additional_history() to include history.
            //
            // for id in read_set.get_delayed_field_keys() {
            //     if !delayed_field_change_set.contains_key(id) {
            //         let read_value = read_set.get_delayed_field_by_kind(id, DelayedFieldReadKind::Bounded).unwrap();
            //     }
            // }

            for (id, change) in delayed_field_change_set.into_iter() {
                prev_modified_delayed_fields.remove(&id);

                let entry = change.into_entry_no_additional_history();

                // TODO[agg_v2](optimize): figure out if it is useful for change to update updates_outside
                if let Err(e) =
                    versioned_cache
                        .delayed_fields()
                        .record_change(id, idx_to_execute, entry)
                {
                    match e {
                        PanicOr::CodeInvariantError(m) => {
                            return Err(code_invariant_error(format!(
                                "Record change failed with CodeInvariantError: {:?}",
                                m
                            )));
                        },
                        PanicOr::Or(_) => {
                            read_set.capture_delayed_field_read_error(&PanicOr::Or(
                                MVDelayedFieldsError::DeltaApplicationFailure,
                            ));
                        },
                    };
                }
            }
            Ok(resource_write_set)
        };

        let (result, resource_write_set) = match execute_result {
            // These statuses are the results of speculative execution, so even for
            // SkipRest (skip the rest of transactions) and Abort (abort execution with
            // user defined error), no immediate action is taken. Instead the statuses
            // are recorded and (final statuses) are analyzed when the block is executed.
            ExecutionStatus::Success(output) => {
                // Apply the writes/deltas to the versioned_data_cache.
                let resource_write_set = apply_updates(&output)?;
                (ExecutionStatus::Success(output), resource_write_set)
            },
            ExecutionStatus::SkipRest(output) => {
                // Apply the writes/deltas and record status indicating skip.
                let resource_write_set = apply_updates(&output)?;
                (ExecutionStatus::SkipRest(output), resource_write_set)
            },
            ExecutionStatus::SpeculativeExecutionAbortError(msg) => {
                read_set.capture_delayed_field_read_error(&PanicOr::Or(
                    MVDelayedFieldsError::DeltaApplicationFailure,
                ));
                (
                    ExecutionStatus::SpeculativeExecutionAbortError(msg),
                    Vec::new(),
                )
            },
            ExecutionStatus::Abort(err) => {
                // Abort indicates an unrecoverable VM failure, but currently it seemingly
                // can occur due to speculative execution (in particular for BlockMetadata txn).
                // Therefore, we do not short circuit here. TODO: investigate if we can
                // eliminate the scenarios when Abort status can happen speculatively.
                (ExecutionStatus::Abort(err), Vec::new())
            },
            ExecutionStatus::DelayedFieldsCodeInvariantError(msg) => {
                return Err(code_invariant_error(format!(
                    "[Execution] At txn {}, failed with DelayedFieldsCodeInvariantError: {:?}",
                    idx_to_execute, msg
                ))
                .into());
            },
        };

        // Remove entries from previous write/delta set that were not overwritten.
        for (k, kind) in prev_modified_keys {
            use KeyKind::*;
            match kind {
                Resource => versioned_cache.data().remove(&k, idx_to_execute),
                Module => versioned_cache.modules().remove(&k, idx_to_execute),
                Group => {
                    versioned_cache.data().remove(&k, idx_to_execute);
                    versioned_cache.group_data().remove(&k, idx_to_execute);
                },
            };
        }

        for id in prev_modified_delayed_fields {
            versioned_cache.delayed_fields().remove(&id, idx_to_execute);
        }

        if !last_input_output.record(idx_to_execute, read_set, result, resource_write_set) {
            // Module R/W is an expected fallback behavior, no alert is required.
            debug!("[Execution] At txn {}, Module read & write", idx_to_execute);

            return Err(PanicOr::Or(
                ParallelBlockExecutionError::ModulePathReadWriteError,
            ));
        }
        Ok(updates_outside)
    }

    fn validate(
        idx_to_validate: TxnIndex,
        last_input_output: &TxnLastInputOutput<T, E::Output, E::Error>,
        versioned_cache: &MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
    ) -> Result<bool, PanicError> {
        let _timer = TASK_VALIDATE_SECONDS.start_timer();
        let read_set = last_input_output
            .read_set(idx_to_validate)
            .expect("[BlockSTM]: Prior read-set must be recorded");

        if read_set.is_incorrect_use() {
            return Err(code_invariant_error(
                "Incorrect use detected in CapturedReads",
            ));
        }

        // Note: we validate delayed field reads only at try_commit.
        // TODO[agg_v2](optimize): potentially add some basic validation.
        // TODO[agg_v2](optimize): potentially add more sophisticated validation, but if it fails,
        // we mark it as a soft failure, requires some new statuses in the scheduler
        // (i.e. not re-execute unless some other part of the validation fails or
        // until commit, but mark as estimates).

        // TODO: validate modules when there is no r/w fallback.
        Ok(
            read_set.validate_data_reads(versioned_cache.data(), idx_to_validate)
                && read_set.validate_group_reads(versioned_cache.group_data(), idx_to_validate),
        )
    }

    fn update_transaction_on_abort(
        txn_idx: TxnIndex,
        last_input_output: &TxnLastInputOutput<T, E::Output, E::Error>,
        versioned_cache: &MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
    ) {
        counters::SPECULATIVE_ABORT_COUNT.inc();

        // Any logs from the aborted execution should be cleared and not reported.
        clear_speculative_txn_logs(txn_idx as usize);

        // Not valid and successfully aborted, mark the latest write/delta sets as estimates.
        if let Some(keys) = last_input_output.modified_keys(txn_idx) {
            for (k, kind) in keys {
                use KeyKind::*;
                match kind {
                    Resource => versioned_cache.data().mark_estimate(&k, txn_idx),
                    Module => versioned_cache.modules().mark_estimate(&k, txn_idx),
                    Group => {
                        versioned_cache.data().mark_estimate(&k, txn_idx);
                        versioned_cache.group_data().mark_estimate(&k, txn_idx);
                    },
                };
            }
        }

        if let Some(keys) = last_input_output.delayed_field_keys(txn_idx) {
            for k in keys {
                versioned_cache.delayed_fields().mark_estimate(&k, txn_idx);
            }
        }
    }

    fn update_on_validation(
        txn_idx: TxnIndex,
        incarnation: Incarnation,
        valid: bool,
        validation_wave: Wave,
        last_input_output: &TxnLastInputOutput<T, E::Output, E::Error>,
        versioned_cache: &MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        scheduler: &Scheduler,
    ) -> Result<SchedulerTask, PanicError> {
        let aborted = !valid && scheduler.try_abort(txn_idx, incarnation);

        if aborted {
            Self::update_transaction_on_abort(txn_idx, last_input_output, versioned_cache);
            scheduler.finish_abort(txn_idx, incarnation)
        } else {
            scheduler.finish_validation(txn_idx, validation_wave);

            if valid {
                scheduler.queueing_commits_arm();
            }

            Ok(SchedulerTask::NoTask)
        }
    }

    fn validate_commit_ready(
        txn_idx: TxnIndex,
        versioned_cache: &MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        last_input_output: &TxnLastInputOutput<T, E::Output, E::Error>,
    ) -> Result<bool, PanicError> {
        let read_set = last_input_output
            .read_set(txn_idx)
            .expect("Read set must be recorded");

        let mut execution_still_valid =
            read_set.validate_delayed_field_reads(versioned_cache.delayed_fields(), txn_idx)?;

        if execution_still_valid {
            if let Some(delayed_field_ids) = last_input_output.delayed_field_keys(txn_idx) {
                if let Err(e) = versioned_cache
                    .delayed_fields()
                    .try_commit(txn_idx, delayed_field_ids.collect())
                {
                    match e {
                        CommitError::ReExecutionNeeded(_) => {
                            execution_still_valid = false;
                        },
                        CommitError::CodeInvariantError(msg) => {
                            return Err(code_invariant_error(msg));
                        },
                    }
                }
            }
        }
        Ok(execution_still_valid)
    }

    /// This method may be executed by different threads / workers, but is guaranteed to be executed
    /// non-concurrently by the scheduling in parallel executor. This allows to perform light logic
    /// related to committing a transaction in a simple way and without excessive synchronization
    /// overhead. On the other hand, materialization that happens after commit (& after this method)
    /// is concurrent and deals with logic such as patching delayed fields / resource groups
    /// in outputs, which is heavier (due to serialization / deserialization, copies, etc). Moreover,
    /// since prepare_and_queue_commit_ready_txns takes care of synchronization in the flag-combining
    /// way, the materialization can be almost embarrassingly parallelizable.
    fn prepare_and_queue_commit_ready_txns(
        &self,
        block_gas_limit_type: &BlockGasLimitType,
        scheduler: &Scheduler,
        versioned_cache: &MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        scheduler_task: &mut SchedulerTask,
        last_input_output: &TxnLastInputOutput<T, E::Output, E::Error>,
        shared_commit_state: &ExplicitSyncWrapper<BlockGasLimitProcessor<T>>,
        base_view: &S,
        start_shared_counter: u32,
        shared_counter: &AtomicU32,
        executor: &E,
        block: &[T],
    ) -> Result<(), PanicOr<ParallelBlockExecutionError>> {
        let mut block_limit_processor = shared_commit_state.acquire();

        while let Some((txn_idx, incarnation)) = scheduler.try_commit() {
            if !Self::validate_commit_ready(txn_idx, versioned_cache, last_input_output)? {
                // Transaction needs to be re-executed, one final time.

                Self::update_transaction_on_abort(txn_idx, last_input_output, versioned_cache);
                // We are going to skip reducing validation index here, as we
                // are executing immediately, and will reduce it unconditionally
                // after execution, inside finish_execution_during_commit.
                // Because of that, we can also ignore _updates_outside result.
                let _updates_outside = Self::execute(
                    txn_idx,
                    incarnation + 1,
                    block,
                    last_input_output,
                    versioned_cache,
                    executor,
                    base_view,
                    ParallelState::new(
                        versioned_cache,
                        scheduler,
                        start_shared_counter,
                        shared_counter,
                    ),
                )?;

                scheduler.finish_execution_during_commit(txn_idx)?;

                let validation_result =
                    Self::validate(txn_idx, last_input_output, versioned_cache)?;
                if !validation_result
                    || !Self::validate_commit_ready(txn_idx, versioned_cache, last_input_output)
                        .unwrap_or(false)
                {
                    return Err(code_invariant_error(format!(
                        "Validation after re-execution failed for {} txn, validate() = {}",
                        txn_idx, validation_result
                    ))
                    .into());
                }
            }

            last_input_output
                .check_fatal_vm_error(txn_idx)
                .map_err(PanicOr::Or)?;
            // Handle a potential vm error, then check invariants on the recorded outputs.
            last_input_output.check_execution_status_during_commit(txn_idx)?;

            if let Some(fee_statement) = last_input_output.fee_statement(txn_idx) {
                let approx_output_size = block_gas_limit_type.block_output_limit().and_then(|_| {
                    last_input_output
                        .output_approx_size(txn_idx)
                        .map(|approx_output| {
                            approx_output
                                + if block_gas_limit_type.include_user_txn_size_in_block_output() {
                                    block[txn_idx as usize].user_txn_bytes_len()
                                } else {
                                    0
                                } as u64
                        })
                });
                let txn_read_write_summary = block_gas_limit_type
                    .conflict_penalty_window()
                    .map(|_| last_input_output.get_txn_read_write_summary(txn_idx));

                // For committed txns with Success status, calculate the accumulated gas costs.
                block_limit_processor.accumulate_fee_statement(
                    fee_statement,
                    txn_read_write_summary,
                    approx_output_size,
                );

                if txn_idx < scheduler.num_txns() - 1
                    && block_limit_processor.should_end_block_parallel()
                {
                    // Set the execution output status to be SkipRest, to skip the rest of the txns.
                    last_input_output.update_to_skip_rest(txn_idx);
                }
            }

            let finalized_groups = groups_to_finalize!(last_input_output, txn_idx)
                .map(|((group_key, metadata_op), is_read_needing_exchange)| {
                    // finalize_group copies Arc of values and the Tags (TODO: optimize as needed).
                    // TODO[agg_v2]: have a test that fails if we don't do the if.
                    let finalized_result = if is_read_needing_exchange {
                        versioned_cache
                            .group_data()
                            .get_last_committed_group(&group_key)
                    } else {
                        versioned_cache
                            .group_data()
                            .finalize_group(&group_key, txn_idx)
                    };
                    map_finalized_group::<T>(
                        group_key,
                        finalized_result,
                        metadata_op,
                        is_read_needing_exchange,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;

            last_input_output.record_finalized_group(txn_idx, finalized_groups);
            defer! {
                scheduler.add_to_commit_queue(txn_idx);
            }

            // While the above propagate errors and lead to eventually halting parallel execution,
            // below we may halt the execution without an error in cases when:
            // a) all transactions are scheduled for committing
            // b) we skip_rest after a transaction
            // Either all txn committed, or a committed txn caused an early halt.
            if txn_idx + 1 == scheduler.num_txns()
                || last_input_output.block_skips_rest_at_idx(txn_idx)
            {
                if txn_idx + 1 == scheduler.num_txns() {
                    assert!(
                        !matches!(scheduler_task, SchedulerTask::ExecutionTask(_, _, _)),
                        "All transactions can be committed, can't have execution task"
                    );
                }

                if scheduler.halt() {
                    block_limit_processor.finish_parallel_update_counters_and_log_info(
                        txn_idx + 1,
                        scheduler.num_txns(),
                    );

                    // failpoint triggering error at the last committed transaction,
                    // to test that next transaction is handled correctly
                    fail_point!("commit-all-halt-err", |_| Err(code_invariant_error(
                        "fail points: Last committed transaction halted"
                    )
                    .into()));
                }
                return Ok(());
            }
        }
        Ok(())
    }

    fn materialize_aggregator_v1_delta_writes(
        txn_idx: TxnIndex,
        last_input_output: &TxnLastInputOutput<T, E::Output, E::Error>,
        versioned_cache: &MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        base_view: &S,
    ) -> Vec<(T::Key, WriteOp)> {
        // Materialize all the aggregator v1 deltas.
        let aggregator_v1_delta_keys = last_input_output.aggregator_v1_delta_keys(txn_idx);
        let mut aggregator_v1_delta_writes = Vec::with_capacity(aggregator_v1_delta_keys.len());
        for k in aggregator_v1_delta_keys.into_iter() {
            // Note that delta materialization happens concurrently, but under concurrent
            // commit_hooks (which may be dispatched by the coordinator), threads may end up
            // contending on delta materialization of the same aggregator. However, the
            // materialization is based on previously materialized values and should not
            // introduce long critical sections. Moreover, with more aggregators, and given
            // that the commit_hook will be performed at dispersed times based on the
            // completion of the respective previous tasks of threads, this should not be
            // an immediate bottleneck - confirmed by an experiment with 32 core and a
            // single materialized aggregator. If needed, the contention may be further
            // mitigated by batching consecutive commit_hooks.
            let committed_delta = versioned_cache
                .data()
                .materialize_delta(&k, txn_idx)
                .unwrap_or_else(|op| {
                    // TODO[agg_v1](cleanup): this logic should improve with the new AGGR data structure
                    // TODO[agg_v1](cleanup): and the ugly base_view parameter will also disappear.
                    let storage_value = base_view
                        .get_state_value(&k)
                        .expect("Error reading the base value for committed delta in storage");

                    let w: T::Value = TransactionWrite::from_state_value(storage_value);
                    let value_u128 = w
                        .as_u128()
                        .expect("Aggregator base value deserialization error")
                        .expect("Aggregator base value must exist");

                    versioned_cache
                        .data()
                        .set_base_value(k.clone(), ValueWithLayout::RawFromStorage(Arc::new(w)));
                    op.apply_to(value_u128)
                        .expect("Materializing delta w. base value set must succeed")
                });

            // Must contain committed value as we set the base value above.
            aggregator_v1_delta_writes.push((
                k,
                WriteOp::legacy_modification(serialize(&committed_delta).into()),
            ));
        }
        aggregator_v1_delta_writes
    }

    fn materialize_txn_commit(
        &self,
        txn_idx: TxnIndex,
        versioned_cache: &MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        scheduler: &Scheduler,
        start_shared_counter: u32,
        shared_counter: &AtomicU32,
        last_input_output: &TxnLastInputOutput<T, E::Output, E::Error>,
        base_view: &S,
        final_results: &ExplicitSyncWrapper<Vec<E::Output>>,
    ) -> Result<(), PanicError> {
        let parallel_state = ParallelState::<T, X>::new(
            versioned_cache,
            scheduler,
            start_shared_counter,
            shared_counter,
        );
        let latest_view = LatestView::new(base_view, ViewState::Sync(parallel_state), txn_idx);
        let finalized_groups = last_input_output.take_finalized_group(txn_idx);
        let materialized_finalized_groups =
            map_id_to_values_in_group_writes(finalized_groups, &latest_view)?;

        let serialized_groups =
            serialize_groups::<T>(materialized_finalized_groups).map_err(|e| {
                code_invariant_error(format!("Panic error in serializing groups {e:?}"))
            })?;

        let resource_write_set = last_input_output.take_resource_write_set(txn_idx);
        let resource_writes_to_materialize = resource_writes_to_materialize!(
            resource_write_set,
            last_input_output,
            versioned_cache.data(),
            txn_idx
        )?;
        let materialized_resource_write_set =
            map_id_to_values_in_write_set(resource_writes_to_materialize, &latest_view)?;

        let events = last_input_output.events(txn_idx);
        let materialized_events = map_id_to_values_events(events, &latest_view)?;
        let aggregator_v1_delta_writes = Self::materialize_aggregator_v1_delta_writes(
            txn_idx,
            last_input_output,
            versioned_cache,
            base_view,
        );

        last_input_output.record_materialized_txn_output(
            txn_idx,
            aggregator_v1_delta_writes,
            materialized_resource_write_set
                .into_iter()
                .chain(serialized_groups)
                .collect(),
            materialized_events,
        )?;
        if let Some(txn_commit_listener) = &self.transaction_commit_hook {
            match last_input_output.txn_output(txn_idx).unwrap().as_ref() {
                ExecutionStatus::Success(output) | ExecutionStatus::SkipRest(output) => {
                    txn_commit_listener.on_transaction_committed(txn_idx, output);
                },
                ExecutionStatus::Abort(_) => {
                    txn_commit_listener.on_execution_aborted(txn_idx);
                },
                ExecutionStatus::SpeculativeExecutionAbortError(msg)
                | ExecutionStatus::DelayedFieldsCodeInvariantError(msg) => {
                    panic!("Cannot be materializing with {}", msg);
                },
            }
        }

        let mut final_results = final_results.acquire();
        match last_input_output.take_output(txn_idx) {
            ExecutionStatus::Success(t) | ExecutionStatus::SkipRest(t) => {
                final_results[txn_idx as usize] = t;
            },
            ExecutionStatus::Abort(_) => (),
            ExecutionStatus::SpeculativeExecutionAbortError(msg)
            | ExecutionStatus::DelayedFieldsCodeInvariantError(msg) => {
                panic!("Cannot be materializing with {}", msg);
            },
        };
        Ok(())
    }

    fn worker_loop(
        &self,
        executor_arguments: &E::Argument,
        block: &[T],
        last_input_output: &TxnLastInputOutput<T, E::Output, E::Error>,
        versioned_cache: &MVHashMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        scheduler: &Scheduler,
        // TODO: should not need to pass base view.
        base_view: &S,
        start_shared_counter: u32,
        shared_counter: &AtomicU32,
        shared_commit_state: &ExplicitSyncWrapper<BlockGasLimitProcessor<T>>,
        final_results: &ExplicitSyncWrapper<Vec<E::Output>>,
    ) -> Result<(), PanicOr<ParallelBlockExecutionError>> {
        // Make executor for each task. TODO: fast concurrent executor.
        let init_timer = VM_INIT_SECONDS.start_timer();
        let executor = E::init(*executor_arguments);
        drop(init_timer);

        let _timer = WORK_WITH_TASK_SECONDS.start_timer();
        let mut scheduler_task = SchedulerTask::NoTask;

        let drain_commit_queue = || -> Result<(), PanicError> {
            while let Ok(txn_idx) = scheduler.pop_from_commit_queue() {
                self.materialize_txn_commit(
                    txn_idx,
                    versioned_cache,
                    scheduler,
                    start_shared_counter,
                    shared_counter,
                    last_input_output,
                    base_view,
                    final_results,
                )?;
            }
            Ok(())
        };

        loop {
            while scheduler.should_coordinate_commits() {
                self.prepare_and_queue_commit_ready_txns(
                    &self.config.onchain.block_gas_limit_type,
                    scheduler,
                    versioned_cache,
                    &mut scheduler_task,
                    last_input_output,
                    shared_commit_state,
                    base_view,
                    start_shared_counter,
                    shared_counter,
                    &executor,
                    block,
                )?;
                scheduler.queueing_commits_mark_done();
            }

            drain_commit_queue()?;

            scheduler_task = match scheduler_task {
                SchedulerTask::ValidationTask(txn_idx, incarnation, wave) => {
                    let valid = Self::validate(txn_idx, last_input_output, versioned_cache)?;
                    Self::update_on_validation(
                        txn_idx,
                        incarnation,
                        valid,
                        wave,
                        last_input_output,
                        versioned_cache,
                        scheduler,
                    )?
                },
                SchedulerTask::ExecutionTask(
                    txn_idx,
                    incarnation,
                    ExecutionTaskType::Execution,
                ) => {
                    let updates_outside = Self::execute(
                        txn_idx,
                        incarnation,
                        block,
                        last_input_output,
                        versioned_cache,
                        &executor,
                        base_view,
                        ParallelState::new(
                            versioned_cache,
                            scheduler,
                            start_shared_counter,
                            shared_counter,
                        ),
                    )?;
                    scheduler.finish_execution(txn_idx, incarnation, updates_outside)?
                },
                SchedulerTask::ExecutionTask(_, _, ExecutionTaskType::Wakeup(condvar)) => {
                    let (lock, cvar) = &*condvar;
                    // Mark dependency resolved.
                    let mut lock = lock.lock();
                    *lock = DependencyStatus::Resolved;
                    // Wake up the process waiting for dependency.
                    cvar.notify_one();

                    scheduler.next_task()
                },
                SchedulerTask::NoTask => scheduler.next_task(),
                SchedulerTask::Done => {
                    drain_commit_queue()?;
                    break Ok(());
                },
            }
        }
    }

    pub(crate) fn execute_transactions_parallel(
        &self,
        executor_initial_arguments: E::Argument,
        signature_verified_block: &[T],
        base_view: &S,
    ) -> Result<BlockOutput<E::Output>, ()> {
        let _timer = PARALLEL_EXECUTION_SECONDS.start_timer();
        // Using parallel execution with 1 thread currently will not work as it
        // will only have a coordinator role but no workers for rolling commit.
        // Need to special case no roles (commit hook by thread itself) to run
        // w. concurrency_level = 1 for some reason.
        assert!(
            self.config.local.concurrency_level > 1,
            "Must use sequential execution"
        );

        let versioned_cache = MVHashMap::new();
        let start_shared_counter = gen_id_start_value(false);
        let shared_counter = AtomicU32::new(start_shared_counter);

        if signature_verified_block.is_empty() {
            return Ok(BlockOutput::new(vec![]));
        }

        let num_txns = signature_verified_block.len();

        let shared_commit_state = ExplicitSyncWrapper::new(BlockGasLimitProcessor::new(
            self.config.onchain.block_gas_limit_type.clone(),
            num_txns,
        ));
        let shared_maybe_error = AtomicBool::new(false);

        let final_results = ExplicitSyncWrapper::new(Vec::with_capacity(num_txns));

        {
            final_results
                .acquire()
                .resize_with(num_txns, E::Output::skip_output);
        }

        let num_txns = num_txns as u32;

        let last_input_output = TxnLastInputOutput::new(num_txns);
        let scheduler = Scheduler::new(num_txns);

        let timer = RAYON_EXECUTION_SECONDS.start_timer();
        self.executor_thread_pool.scope(|s| {
            for _ in 0..self.config.local.concurrency_level {
                s.spawn(|_| {
                    if let Err(err) = self.worker_loop(
                        &executor_initial_arguments,
                        signature_verified_block,
                        &last_input_output,
                        &versioned_cache,
                        &scheduler,
                        base_view,
                        start_shared_counter,
                        &shared_counter,
                        &shared_commit_state,
                        &final_results,
                    ) {
                        // If there are multiple errors, they all get logged:
                        // ModulePathReadWriteError and FatalVMErrorvariant is logged at construction,
                        // and below we log CodeInvariantErrors.
                        if let PanicOr::CodeInvariantError(err_msg) = err {
                            alert!("[BlockSTM] worker loop: CodeInvariantError({:?})", err_msg);
                        }
                        shared_maybe_error.store(true, Ordering::SeqCst);

                        // Make sure to halt the scheduler if it hasn't already been halted.
                        scheduler.halt();
                    }
                });
            }
        });
        drop(timer);
        // Explicit async drops.
        DEFAULT_DROPPER.schedule_drop((last_input_output, scheduler, versioned_cache));

        // TODO add block end info to output.
        // block_limit_processor.is_block_limit_reached();

        (!shared_maybe_error.load(Ordering::SeqCst))
            .then(|| BlockOutput::new(final_results.into_inner()))
            .ok_or(())
    }

    fn apply_output_sequential(
        unsync_map: &UnsyncMap<T::Key, T::Tag, T::Value, X, T::Identifier>,
        output: &E::Output,
        resource_write_set: Vec<(T::Key, Arc<T::Value>, Option<Arc<MoveTypeLayout>>)>,
    ) -> Result<(), SequentialBlockExecutionError<E::Error>> {
        for (key, write_op, layout) in resource_write_set.into_iter() {
            unsync_map.write(key, write_op, layout);
        }

        for (group_key, metadata_op, group_ops) in output.resource_group_write_set().into_iter() {
            for (value_tag, (group_op, maybe_layout)) in group_ops.into_iter() {
                unsync_map.insert_group_op(&group_key, value_tag, group_op, maybe_layout)?;
            }
            unsync_map.write(group_key, Arc::new(metadata_op), None);
        }

        for (key, write_op) in output.aggregator_v1_write_set().into_iter() {
            unsync_map.write(key, Arc::new(write_op), None);
        }

        for (key, write_op) in output.module_write_set().into_iter() {
            unsync_map.write_module(key, write_op);
        }

        let mut second_phase = Vec::new();
        let mut updates = HashMap::new();
        for (id, change) in output.delayed_field_change_set().into_iter() {
            match change {
                DelayedChange::Create(value) => {
                    assert_none!(
                        unsync_map.fetch_delayed_field(&id),
                        "Sequential execution must not create duplicate aggregators"
                    );
                    updates.insert(id, value);
                },
                DelayedChange::Apply(apply) => {
                    match apply.get_apply_base_id(&id) {
                        ApplyBase::Previous(base_id) => {
                            updates.insert(
                                id,
                                expect_ok(apply.apply_to_base(
                                    unsync_map.fetch_delayed_field(&base_id).unwrap(),
                                ))
                                .unwrap(),
                            );
                        },
                        ApplyBase::Current(base_id) => {
                            second_phase.push((id, base_id, apply));
                        },
                    };
                },
            }
        }
        for (id, base_id, apply) in second_phase.into_iter() {
            updates.insert(
                id,
                expect_ok(
                    apply.apply_to_base(
                        updates
                            .get(&base_id)
                            .cloned()
                            .unwrap_or_else(|| unsync_map.fetch_delayed_field(&base_id).unwrap()),
                    ),
                )
                .unwrap(),
            );
        }
        for (id, value) in updates.into_iter() {
            unsync_map.write_delayed_field(id, value);
        }

        Ok(())
    }

    pub(crate) fn execute_transactions_sequential(
        &self,
        executor_arguments: E::Argument,
        signature_verified_block: &[T],
        base_view: &S,
        resource_group_bcs_fallback: bool,
    ) -> Result<BlockOutput<E::Output>, SequentialBlockExecutionError<E::Error>> {
        let num_txns = signature_verified_block.len();
        let init_timer = VM_INIT_SECONDS.start_timer();
        let executor = E::init(executor_arguments);
        drop(init_timer);

        let start_counter = gen_id_start_value(true);
        let counter = RefCell::new(start_counter);
        let unsync_map = UnsyncMap::new();
        let mut ret = Vec::with_capacity(num_txns);
        let mut block_limit_processor = BlockGasLimitProcessor::<T>::new(
            self.config.onchain.block_gas_limit_type.clone(),
            num_txns,
        );

        let last_input_output: TxnLastInputOutput<T, E::Output, E::Error> =
            TxnLastInputOutput::new(num_txns as TxnIndex);

        for (idx, txn) in signature_verified_block.iter().enumerate() {
            let latest_view = LatestView::<T, S, X>::new(
                base_view,
                ViewState::Unsync(SequentialState::new(&unsync_map, start_counter, &counter)),
                idx as TxnIndex,
            );
            let res = executor.execute_transaction(&latest_view, txn, idx as TxnIndex);
            let must_skip = matches!(res, ExecutionStatus::SkipRest(_));
            match res {
                ExecutionStatus::Abort(err) => {
                    if let Some(commit_hook) = &self.transaction_commit_hook {
                        commit_hook.on_execution_aborted(idx as TxnIndex);
                    }
                    error!(
                        "Sequential execution FatalVMError by transaction {}",
                        idx as TxnIndex
                    );
                    // Record the status indicating the unrecoverable VM failure.
                    return Err(SequentialBlockExecutionError::ErrorToReturn(
                        BlockExecutionError::FatalVMError(err),
                    ));
                },
                ExecutionStatus::DelayedFieldsCodeInvariantError(msg) => {
                    if let Some(commit_hook) = &self.transaction_commit_hook {
                        commit_hook.on_execution_aborted(idx as TxnIndex);
                    }
                    alert!("Sequential execution DelayedFieldsCodeInvariantError error by transaction {}: {}", idx as TxnIndex, msg);
                    return Err(SequentialBlockExecutionError::ErrorToReturn(
                        BlockExecutionError::FatalBlockExecutorError(code_invariant_error(msg)),
                    ));
                },
                ExecutionStatus::SpeculativeExecutionAbortError(msg) => {
                    if let Some(commit_hook) = &self.transaction_commit_hook {
                        commit_hook.on_execution_aborted(idx as TxnIndex);
                    }
                    alert!("Sequential execution SpeculativeExecutionAbortError error by transaction {}: {}", idx as TxnIndex, msg);
                    return Err(SequentialBlockExecutionError::ErrorToReturn(
                        BlockExecutionError::FatalBlockExecutorError(code_invariant_error(msg)),
                    ));
                },
                ExecutionStatus::Success(output) | ExecutionStatus::SkipRest(output) => {
                    // Calculating the accumulated gas costs of the committed txns.
                    let fee_statement = output.fee_statement();

                    let approx_output_size = self
                        .config
                        .onchain
                        .block_gas_limit_type
                        .block_output_limit()
                        .map(|_| {
                            output.output_approx_size()
                                + if self
                                    .config
                                    .onchain
                                    .block_gas_limit_type
                                    .include_user_txn_size_in_block_output()
                                {
                                    txn.user_txn_bytes_len()
                                } else {
                                    0
                                } as u64
                        });

                    let sequential_reads = latest_view.take_sequential_reads();
                    let read_write_summary = self
                        .config
                        .onchain
                        .block_gas_limit_type
                        .conflict_penalty_window()
                        .map(|_| {
                            ReadWriteSummary::new(
                                sequential_reads.get_read_summary(),
                                output.get_write_summary(),
                            )
                        });

                    if last_input_output.check_and_append_module_rw_conflict(
                        sequential_reads.module_reads.iter(),
                        output.module_write_set().keys(),
                    ) {
                        block_limit_processor.process_module_rw_conflict();
                    }

                    block_limit_processor.accumulate_fee_statement(
                        fee_statement,
                        read_write_summary,
                        approx_output_size,
                    );

                    output.materialize_agg_v1(&latest_view);
                    assert_eq!(
                        output.aggregator_v1_delta_set().len(),
                        0,
                        "Sequential execution must materialize deltas"
                    );

                    if resource_group_bcs_fallback {
                        // Dynamic change set optimizations are enabled, and resource group serialization
                        // previously failed in bcs serialization for preparing final transaction outputs.
                        // TODO: remove this fallback when txn errors can be created from block executor.

                        let finalize = |group_key| -> BTreeMap<_, _> {
                            unsync_map
                                .finalize_group(&group_key)
                                .map(|(resource_tag, value_with_layout)| {
                                    let value = match value_with_layout {
                                        ValueWithLayout::RawFromStorage(value)
                                        | ValueWithLayout::Exchanged(value, _) => value,
                                    };
                                    (
                                        resource_tag,
                                        value
                                            .extract_raw_bytes()
                                            .expect("Deletions should already be applied"),
                                    )
                                })
                                .collect()
                        };

                        // The IDs are not exchanged but it doesn't change the types (Bytes) or size.
                        let serialization_error = output
                            .group_reads_needing_delayed_field_exchange()
                            .iter()
                            .any(|(group_key, _)| {
                                fail_point!("fail-point-resource-group-serialization", |_| {
                                    true
                                });

                                let finalized_group = finalize(group_key.clone());
                                bcs::to_bytes(&finalized_group).is_err()
                            })
                            || output.resource_group_write_set().into_iter().any(
                                |(group_key, _, group_ops)| {
                                    fail_point!("fail-point-resource-group-serialization", |_| {
                                        true
                                    });

                                    let mut finalized_group = finalize(group_key);
                                    for (value_tag, (group_op, _)) in group_ops {
                                        if group_op.is_deletion() {
                                            finalized_group.remove(&value_tag);
                                        } else {
                                            finalized_group.insert(
                                                value_tag,
                                                group_op
                                                    .extract_raw_bytes()
                                                    .expect("Not a deletion"),
                                            );
                                        }
                                    }
                                    bcs::to_bytes(&finalized_group).is_err()
                                },
                            );

                        if serialization_error {
                            // The corresponding error / alert must already be triggered, the goal in sequential
                            // fallback is to just skip any transactions that would cause such serialization errors.
                            alert!("Discarding transaction because serialization failed in bcs fallback");
                            ret.push(E::Output::discard_output(
                                StatusCode::DELAYED_MATERIALIZATION_CODE_INVARIANT_ERROR,
                            ));
                            continue;
                        }
                    };

                    // Apply the writes.
                    let resource_write_set = output.resource_write_set();
                    Self::apply_output_sequential(
                        &unsync_map,
                        &output,
                        resource_write_set.clone(),
                    )?;

                    // If dynamic change set materialization part (indented for clarity/variable scope):
                    {
                        let finalized_groups = groups_to_finalize!(output,)
                            .map(|((group_key, metadata_op), is_read_needing_exchange)| {
                                let finalized_group =
                                    Ok(unsync_map.finalize_group(&group_key).collect());
                                map_finalized_group::<T>(
                                    group_key,
                                    finalized_group,
                                    metadata_op,
                                    is_read_needing_exchange,
                                )
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        let materialized_finalized_groups =
                            map_id_to_values_in_group_writes(finalized_groups, &latest_view)?;
                        let serialized_groups =
                            serialize_groups::<T>(materialized_finalized_groups).map_err(|_| {
                                SequentialBlockExecutionError::ResourceGroupSerializationError
                            })?;

                        let resource_writes_to_materialize = resource_writes_to_materialize!(
                            resource_write_set,
                            output,
                            unsync_map,
                        )?;
                        // Replace delayed field id with values in resource write set and read set.
                        let materialized_resource_write_set = map_id_to_values_in_write_set(
                            resource_writes_to_materialize,
                            &latest_view,
                        )?;

                        // Replace delayed field id with values in events
                        let materialized_events = map_id_to_values_events(
                            Box::new(output.get_events().into_iter()),
                            &latest_view,
                        )?;

                        output.incorporate_materialized_txn_output(
                            // No aggregator v1 delta writes are needed for sequential execution.
                            // They are already handled because we passed materialize_deltas=true
                            // to execute_transaction.
                            vec![],
                            materialized_resource_write_set
                                .into_iter()
                                .chain(serialized_groups.into_iter())
                                .collect(),
                            materialized_events,
                        )?;
                    }
                    // If dynamic change set is disabled, this can be used to assert nothing needs patching instead:
                    //   output.set_txn_output_for_non_dynamic_change_set();

                    if latest_view.is_incorrect_use() {
                        return Err(
                            code_invariant_error("Incorrect use in sequential execution").into(),
                        );
                    }

                    if let Some(commit_hook) = &self.transaction_commit_hook {
                        commit_hook.on_transaction_committed(idx as TxnIndex, &output);
                    }
                    ret.push(output);
                },
            };
            // When the txn is a SkipRest txn, halt sequential execution.
            if must_skip {
                break;
            }

            if idx < num_txns - 1 && block_limit_processor.should_end_block_sequential() {
                break;
            }
        }

        block_limit_processor
            .finish_sequential_update_counters_and_log_info(ret.len() as u32, num_txns as u32);

        ret.resize_with(num_txns, E::Output::skip_output);

        // TODO add block end info to output.
        // block_limit_processor.is_block_limit_reached();

        Ok(BlockOutput::new(ret))
    }

    pub fn execute_block(
        &self,
        executor_arguments: E::Argument,
        signature_verified_block: &[T],
        base_view: &S,
    ) -> BlockExecutionResult<BlockOutput<E::Output>, E::Error> {
        if self.config.local.concurrency_level > 1 {
            let parallel_result = self.execute_transactions_parallel(
                executor_arguments,
                signature_verified_block,
                base_view,
            );

            // If parallel gave us result, return it
            if let Ok(output) = parallel_result {
                return Ok(output);
            }

            if !self.config.local.allow_fallback {
                panic!("Parallel execution failed and fallback is not allowed");
            }

            // All logs from the parallel execution should be cleared and not reported.
            // Clear by re-initializing the speculative logs.
            init_speculative_logs(signature_verified_block.len());

            info!("parallel execution requiring fallback");
        }

        // If we didn't run parallel or it didn't finish successfully - run sequential
        let sequential_result = self.execute_transactions_sequential(
            executor_arguments,
            signature_verified_block,
            base_view,
            false,
        );

        // If sequential gave us result, return it
        let sequential_error = match sequential_result {
            Ok(output) => {
                return Ok(output);
            },
            Err(SequentialBlockExecutionError::ResourceGroupSerializationError) => {
                if !self.config.local.allow_fallback {
                    panic!("Parallel execution failed and fallback is not allowed");
                }

                // TODO[agg_v2](cleanup): check if sequential execution logs anything in the speculative logs,
                // and whether clearing them below is needed at all.
                // All logs from the first pass of sequential execution should be cleared and not reported.
                // Clear by re-initializing the speculative logs.
                init_speculative_logs(signature_verified_block.len());

                let sequential_result = self.execute_transactions_sequential(
                    executor_arguments,
                    signature_verified_block,
                    base_view,
                    true,
                );

                // If sequential gave us result, return it
                match sequential_result {
                    Ok(output) => {
                        return Ok(output);
                    },
                    Err(SequentialBlockExecutionError::ResourceGroupSerializationError) => {
                        BlockExecutionError::FatalBlockExecutorError(code_invariant_error(
                            "resource group serialization during bcs fallback should not happen",
                        ))
                    },
                    Err(SequentialBlockExecutionError::ErrorToReturn(err)) => err,
                }
            },
            Err(SequentialBlockExecutionError::ErrorToReturn(err)) => err,
        };

        if self.config.local.discard_failed_blocks {
            // We cannot execute block, discard everything (including block metadata and validator transactions)
            // (TODO: maybe we should add fallback here to first try BlockMetadataTransaction alone)
            // StateCheckpoint will be added afterwards.
            let error_code = match sequential_error {
                BlockExecutionError::FatalBlockExecutorError(_) => {
                    StatusCode::DELAYED_MATERIALIZATION_CODE_INVARIANT_ERROR
                },
                BlockExecutionError::FatalVMError(_) => {
                    StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR
                },
            };
            let ret = signature_verified_block
                .iter()
                .map(|_| E::Output::discard_output(error_code))
                .collect();
            return Ok(BlockOutput::new(ret));
        }

        Err(sequential_error)
    }
}
