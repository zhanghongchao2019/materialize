// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A controller for a compute instance.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::num::NonZeroI64;

use differential_dataflow::lattice::Lattice;
use futures::stream::FuturesUnordered;
use futures::{future, StreamExt};
use thiserror::Error;
use timely::progress::{Antichain, ChangeBatch, Timestamp};
use timely::PartialOrder;
use uuid::Uuid;

use mz_build_info::BuildInfo;
use mz_expr::RowSetFinishing;
use mz_ore::tracing::OpenTelemetryContext;
use mz_repr::{GlobalId, Row};
use mz_storage_client::controller::{ReadPolicy, StorageController};

use crate::command::{
    ComputeCommand, ComputeCommandHistory, ComputeStartupEpoch, DataflowDescription,
    InstanceConfig, Peek, ReplicaId, SourceInstanceDesc,
};
use crate::logging::{LogVariant, LoggingConfig};
use crate::response::{ComputeResponse, PeekResponse, SubscribeBatch, SubscribeResponse};
use crate::service::{ComputeClient, ComputeGrpcClient};
use crate::sinks::{ComputeSinkConnection, ComputeSinkDesc, PersistSinkConnection};

use super::error::CollectionMissing;
use super::orchestrator::ComputeOrchestrator;
use super::replica::Replica;
use super::{
    CollectionState, ComputeControllerResponse, ComputeInstanceId, ComputeReplicaLocation,
};

#[derive(Error, Debug)]
#[error("replica exists already: {0}")]
pub(super) struct ReplicaExists(pub ReplicaId);

#[derive(Error, Debug)]
#[error("replica does not exist: {0}")]
pub(super) struct ReplicaMissing(pub ReplicaId);

#[derive(Error, Debug)]
pub(super) enum DataflowCreationError {
    #[error("collection does not exist: {0}")]
    CollectionMissing(GlobalId),
    #[error("dataflow definition lacks an as_of value")]
    MissingAsOf,
    #[error("dataflow has an as_of not beyond the since of collection: {0}")]
    SinceViolation(GlobalId),
}

impl From<CollectionMissing> for DataflowCreationError {
    fn from(error: CollectionMissing) -> Self {
        Self::CollectionMissing(error.0)
    }
}

#[derive(Error, Debug)]
pub(super) enum PeekError {
    #[error("collection does not exist: {0}")]
    CollectionMissing(GlobalId),
    #[error("peek timestamp is not beyond the since of collection: {0}")]
    SinceViolation(GlobalId),
}

impl From<CollectionMissing> for PeekError {
    fn from(error: CollectionMissing) -> Self {
        Self::CollectionMissing(error.0)
    }
}

/// The state we keep for a compute instance.
#[derive(Debug)]
pub(super) struct Instance<T> {
    /// ID of this instance
    instance_id: ComputeInstanceId,
    /// Build info for spawning replicas
    build_info: &'static BuildInfo,
    /// The replicas of this compute instance.
    replicas: HashMap<ReplicaId, Replica<T>>,
    /// Tracks expressed `since` and received `upper` frontiers for indexes and sinks.
    collections: BTreeMap<GlobalId, CollectionState<T>>,
    /// IDs of arranged log sources maintained by this compute instance.
    arranged_logs: BTreeMap<LogVariant, GlobalId>,
    /// Currently outstanding peeks.
    peeks: HashMap<Uuid, PendingPeek<T>>,
    /// Frontiers of in-progress subscribes.
    subscribes: BTreeMap<GlobalId, Antichain<T>>,
    /// The command history, used when introducing new replicas or restarting existing replicas.
    history: ComputeCommandHistory<T>,
    /// IDs of replicas that have failed and require rehydration.
    failed_replicas: BTreeSet<ReplicaId>,
    /// Ready compute controller responses to be delivered.
    pub ready_responses: VecDeque<ComputeControllerResponse<T>>,
    /// Orchestrator for managing replicas
    orchestrator: ComputeOrchestrator,
    /// A number that increases with each restart of `environmentd`.
    envd_epoch: NonZeroI64,
    /// Numbers that increase with each restart of a replica.
    replica_epochs: HashMap<ReplicaId, u64>,
}

impl<T> Instance<T> {
    /// Acquire a handle to the collection state associated with `id`.
    pub fn collection(&self, id: GlobalId) -> Result<&CollectionState<T>, CollectionMissing> {
        self.collections.get(&id).ok_or(CollectionMissing(id))
    }

    /// Acquire a mutable handle to the collection state associated with `id`.
    fn collection_mut(
        &mut self,
        id: GlobalId,
    ) -> Result<&mut CollectionState<T>, CollectionMissing> {
        self.collections.get_mut(&id).ok_or(CollectionMissing(id))
    }

    /// Return the IDs of pending peeks targeting the specified replica.
    fn peeks_targeting(
        &self,
        replica_id: ReplicaId,
    ) -> impl Iterator<Item = (Uuid, &PendingPeek<T>)> {
        self.peeks.iter().filter_map(move |(uuid, peek)| {
            if peek.target_replica == Some(replica_id) {
                Some((*uuid, peek))
            } else {
                None
            }
        })
    }

    /// Acquire an [`ActiveInstance`] by providing a storage controller.
    pub fn activate<'a>(
        &'a mut self,
        storage_controller: &'a mut dyn StorageController<Timestamp = T>,
    ) -> ActiveInstance<'a, T> {
        ActiveInstance {
            compute: self,
            storage_controller,
        }
    }

    /// Return whether this instance has any processing work scheduled.
    pub fn wants_processing(&self) -> bool {
        // Do we need to rehydrate failed replicas?
        !self.failed_replicas.is_empty()
        // Do we have responses ready to deliver?
        || !self.ready_responses.is_empty()
    }

    /// Returns the ids of all replicas of this instance
    pub fn replica_ids(&self) -> impl Iterator<Item = &ReplicaId> {
        self.replicas.keys()
    }
}

impl<T> Instance<T>
where
    T: Timestamp + Lattice,
    ComputeGrpcClient: ComputeClient<T>,
{
    pub fn new(
        instance_id: ComputeInstanceId,
        build_info: &'static BuildInfo,
        arranged_logs: BTreeMap<LogVariant, GlobalId>,
        max_result_size: u32,
        orchestrator: ComputeOrchestrator,
        envd_epoch: NonZeroI64,
    ) -> Self {
        let collections = arranged_logs
            .iter()
            .map(|(_, id)| {
                let state = CollectionState::new_log_collection();
                (*id, state)
            })
            .collect();

        let mut instance = Self {
            instance_id,
            build_info,
            replicas: Default::default(),
            collections,
            arranged_logs,
            peeks: Default::default(),
            subscribes: Default::default(),
            history: Default::default(),
            failed_replicas: Default::default(),
            ready_responses: Default::default(),
            orchestrator,
            envd_epoch,
            replica_epochs: Default::default(),
        };

        instance.send(ComputeCommand::CreateTimely {
            comm_config: Default::default(),
            epoch: ComputeStartupEpoch::new(envd_epoch, 0),
        });
        instance.send(ComputeCommand::CreateInstance(InstanceConfig {
            logging: Default::default(),
            max_result_size,
        }));

        instance
    }

    /// Marks the end of any initialization commands.
    ///
    /// Intended to be called by `Controller`, rather than by other code (to avoid repeated calls).
    pub fn initialization_complete(&mut self) {
        self.send(ComputeCommand::InitializationComplete);
    }

    /// Drop this compute instance.
    ///
    /// # Panics
    ///
    /// Panics if the compute instance still has active replicas.
    pub fn drop(self) {
        assert!(
            self.replicas.is_empty(),
            "cannot drop instances with provisioned replicas"
        );
    }

    /// Sends a command to all replicas of this instance.
    #[tracing::instrument(level = "debug", skip(self))]
    pub fn send(&mut self, cmd: ComputeCommand<T>) {
        // Record the command so that new replicas can be brought up to speed.
        self.history.push(cmd.clone(), &self.peeks);

        // Clone the command for each active replica.
        for (id, replica) in self.replicas.iter_mut() {
            // If sending the command fails, the replica requires rehydration.
            if replica.send(cmd.clone()).is_err() {
                self.failed_replicas.insert(*id);
            }
        }
    }

    /// Receives the next response from any replica of this instance.
    ///
    /// Returns `Err` if receiving from a replica has failed, to signal that it is in need of
    /// rehydration.
    ///
    /// This method is cancellation safe.
    pub async fn recv(&mut self) -> Result<(ReplicaId, ComputeResponse<T>), ReplicaId> {
        // Receive responses from any of the replicas, and take appropriate
        // action.
        let response = self
            .replicas
            .iter_mut()
            .map(|(id, replica)| async { (*id, replica.recv().await) })
            .collect::<FuturesUnordered<_>>()
            .next()
            .await;

        match response {
            None => {
                // There were no replicas in the set. Block forever to
                // communicate that no response is ready.
                future::pending().await
            }
            Some((replica_id, None)) => {
                // A replica has failed and requires rehydration.
                self.failed_replicas.insert(replica_id);
                Err(replica_id)
            }
            Some((replica_id, Some(response))) => {
                // A replica has produced a response. Return it.
                Ok((replica_id, response))
            }
        }
    }
}

/// A wrapper around [`Instance`] with a live storage controller.
#[derive(Debug)]
pub(super) struct ActiveInstance<'a, T> {
    compute: &'a mut Instance<T>,
    storage_controller: &'a mut dyn StorageController<Timestamp = T>,
}

impl<'a, T> ActiveInstance<'a, T>
where
    T: Timestamp + Lattice,
    ComputeGrpcClient: ComputeClient<T>,
{
    /// Add a new instance replica, by ID.
    pub fn add_replica(
        &mut self,
        id: ReplicaId,
        location: ComputeReplicaLocation,
        mut logging_config: LoggingConfig,
    ) -> Result<(), ReplicaExists> {
        if self.compute.replicas.contains_key(&id) {
            return Err(ReplicaExists(id));
        }

        // Initialize state for per-replica log collections.
        for (log_id, _) in logging_config.sink_logs.values() {
            self.compute
                .collections
                .insert(*log_id, CollectionState::new_log_collection());
        }

        logging_config.index_logs = self.compute.arranged_logs.clone();
        let maintained_logs: BTreeSet<_> = logging_config.log_identifiers().collect();

        // Initialize frontier tracking for the new replica
        // and clean up any dropped collections that we can
        let mut updates = Vec::new();
        for (compute_id, collection) in &mut self.compute.collections {
            // Skip log collections not maintained by this replica.
            if collection.log_collection && !maintained_logs.contains(compute_id) {
                continue;
            }

            let read_frontier = collection.read_frontier();
            updates.push((*compute_id, read_frontier.to_owned()));
        }
        self.update_write_frontiers(id, &updates);

        let replica_epoch = self.compute.replica_epochs.entry(id).or_default();
        *replica_epoch += 1;
        let replica = Replica::spawn(
            id,
            self.compute.instance_id,
            self.compute.build_info,
            location,
            logging_config,
            self.compute.orchestrator.clone(),
            ComputeStartupEpoch::new(self.compute.envd_epoch, *replica_epoch),
        );

        // Take this opportunity to clean up the history we should present.
        self.compute.history.retain_peeks(&self.compute.peeks);
        self.compute.history.reduce();

        // Replay the commands at the client, creating new dataflow identifiers.
        for command in self.compute.history.iter() {
            if replica.send(command.clone()).is_err() {
                // We swallow the error here. On the next send, we will fail again, and
                // restart the connection as well as this rehydration.
                tracing::warn!("Replica {:?} connection terminated during hydration", id);
                break;
            }
        }

        // Add replica to tracked state.
        self.compute.replicas.insert(id, replica);

        Ok(())
    }

    /// Remove an existing instance replica, by ID.
    ///
    /// This method removes the replica from the orchestrator and should only be called if the
    /// replica should be permanently removed.
    pub fn remove_replica(&mut self, id: ReplicaId) -> Result<(), ReplicaMissing> {
        let replica = self
            .compute
            .replicas
            .get_mut(&id)
            .ok_or(ReplicaMissing(id))?;

        // If the replica is managed we have to remove it from the orchestrator. We spawn
        // a background task that waits until the termination of the message handler task and
        // then removes it from the orchestrator.
        if matches!(replica.location, ComputeReplicaLocation::Managed { .. }) {
            let replica_task = replica.replica_task.take().unwrap();
            let instance_id = self.compute.instance_id;
            let orchestrator = self.compute.orchestrator.clone();
            mz_ore::task::spawn(|| format!("drop-replica-{id}"), async move {
                // Ensure the active-replication-replica task is terminated before removing the service
                // from the orchestrator. This await guarantees the ensure call has happened before
                // we remove the replica from the orchestrator.
                replica_task.abort();
                let join_result = replica_task.await;
                tracing::debug!("Replica task joined: {:?}", join_result);

                match orchestrator.drop_replica(instance_id, id).await {
                    Ok(_) => {
                        tracing::debug!("Removed replica from orchestrator")
                    }
                    Err(e) => {
                        tracing::warn!("Could not drop replica {:?}: {}", &id, &e)
                    }
                }
            });
        }

        // Peeks targeting this replica won't be served anymore now. We return an error for them to
        // avoid leaving them pending forever.
        let mut peek_responses = Vec::new();
        let mut peek_ids = BTreeSet::new();
        for (uuid, peek) in self.compute.peeks_targeting(id) {
            peek_responses.push(ComputeControllerResponse::PeekResponse(
                uuid,
                PeekResponse::Error("target replica was dropped".into()),
                peek.otel_ctx.clone(),
            ));
            peek_ids.insert(uuid);
        }
        self.compute.ready_responses.extend(peek_responses);
        self.remove_peeks(&peek_ids);

        self.remove_replica_state(id);
        Ok(())
    }

    /// Remove all state related to a replica.
    ///
    /// This method does not cause an orchestrator removal of the replica, so it is suitable for
    /// removing the replica temporarily, e.g., during rehydration.
    ///
    /// # Panics
    ///
    /// Panics if the specified replica does not exist in the compute state.
    fn remove_replica_state(&mut self, id: ReplicaId) {
        // Remove frontier tracking for this replica.
        self.remove_write_frontiers(id);

        self.compute
            .replicas
            .remove(&id)
            .expect("replica not found");

        // In case the replica crashes and we receive a drop replica request
        // at the same time, the cleanup request will race with the rehydration.
        // Hence we also have to stop a pending rehydration request.
        self.compute.failed_replicas.remove(&id);
    }

    fn rehydrate_replica(&mut self, id: ReplicaId) {
        let location = self.compute.replicas[&id].location.clone();
        let logging_config = self.compute.replicas[&id].logging_config.clone();
        self.remove_replica_state(id);
        let result = self.add_replica(id, location, logging_config);

        match result {
            Ok(()) => (),
            Err(ReplicaExists(_)) => unreachable!("replica was removed"),
        }
    }

    /// Rehydrate any failed replicas of this instance.
    pub fn rehydrate_failed_replicas(&mut self) {
        let failed_replicas = self.compute.failed_replicas.clone();
        for replica_id in failed_replicas {
            self.rehydrate_replica(replica_id);
            self.compute.failed_replicas.remove(&replica_id);
        }
    }

    /// Create the described dataflows and initializes state for their output.
    pub fn create_dataflows(
        &mut self,
        dataflows: Vec<DataflowDescription<crate::plan::Plan<T>, (), T>>,
    ) -> Result<(), DataflowCreationError> {
        // Validate dataflows as having inputs whose `since` is less or equal to the dataflow's `as_of`.
        // Start tracking frontiers for each dataflow, using its `as_of` for each index and sink.
        for dataflow in dataflows.iter() {
            let as_of = dataflow
                .as_of
                .as_ref()
                .ok_or(DataflowCreationError::MissingAsOf)?;

            // Record all transitive dependencies of the outputs.
            let mut storage_dependencies = Vec::new();
            let mut compute_dependencies = Vec::new();

            // Validate sources have `since.less_equal(as_of)`.
            for source_id in dataflow.source_imports.keys() {
                let since = &self
                    .storage_controller
                    .collection(*source_id)
                    .map_err(|_| DataflowCreationError::CollectionMissing(*source_id))?
                    .read_capabilities
                    .frontier();
                if !(timely::order::PartialOrder::less_equal(since, &as_of.borrow())) {
                    Err(DataflowCreationError::SinceViolation(*source_id))?;
                }

                storage_dependencies.push(*source_id);
            }

            // Validate indexes have `since.less_equal(as_of)`.
            // TODO(mcsherry): Instead, return an error from the constructing method.
            for index_id in dataflow.index_imports.keys() {
                let collection = self.compute.collection(*index_id)?;
                let since = collection.read_capabilities.frontier();
                if !(timely::order::PartialOrder::less_equal(&since, &as_of.borrow())) {
                    Err(DataflowCreationError::SinceViolation(*index_id))?;
                } else {
                    compute_dependencies.push(*index_id);
                }
            }

            // Canonicalize dependencies.
            // Probably redundant based on key structure, but doing for sanity.
            storage_dependencies.sort();
            storage_dependencies.dedup();
            compute_dependencies.sort();
            compute_dependencies.dedup();

            // We will bump the internals of each input by the number of dependents (outputs).
            let outputs = dataflow.sink_exports.len() + dataflow.index_exports.len();
            let mut changes = ChangeBatch::new();
            for time in as_of.iter() {
                changes.update(time.clone(), outputs as i64);
            }
            // Update storage read capabilities for inputs.
            let mut storage_read_updates = storage_dependencies
                .iter()
                .map(|id| (*id, changes.clone()))
                .collect();
            self.storage_controller
                .update_read_capabilities(&mut storage_read_updates);
            // Update compute read capabilities for inputs.
            let mut compute_read_updates = compute_dependencies
                .iter()
                .map(|id| (*id, changes.clone()))
                .collect();
            self.update_read_capabilities(&mut compute_read_updates);

            // Install collection state for each of the exports.
            let mut updates = Vec::new();
            for export_id in dataflow.export_ids() {
                self.compute.collections.insert(
                    export_id,
                    CollectionState::new(
                        as_of.clone(),
                        storage_dependencies.clone(),
                        compute_dependencies.clone(),
                    ),
                );
                updates.push((export_id, as_of.clone()));
            }
            // Initialize tracking of replica frontiers.
            let replica_ids: Vec<_> = self.compute.replicas.keys().copied().collect();
            for replica_id in replica_ids {
                self.update_write_frontiers(replica_id, &updates);
            }

            // Initialize tracking of subscribes.
            for subscribe_id in dataflow.subscribe_ids() {
                self.compute
                    .subscribes
                    .insert(subscribe_id, Antichain::from_elem(Timestamp::minimum()));
            }
        }

        // Here we augment all imported sources and all exported sinks with with the appropriate
        // storage metadata needed by the compute instance.
        let mut augmented_dataflows = Vec::with_capacity(dataflows.len());
        for d in dataflows {
            let mut source_imports = BTreeMap::new();
            for (id, (si, monotonic)) in d.source_imports {
                let collection = self
                    .storage_controller
                    .collection(id)
                    .map_err(|_| DataflowCreationError::CollectionMissing(id))?;
                let desc = SourceInstanceDesc {
                    storage_metadata: collection.collection_metadata.clone(),
                    arguments: si.arguments,
                    typ: collection.description.desc.typ().clone(),
                };
                source_imports.insert(id, (desc, monotonic));
            }

            let mut sink_exports = BTreeMap::new();
            for (id, se) in d.sink_exports {
                let connection = match se.connection {
                    ComputeSinkConnection::Persist(conn) => {
                        let metadata = self
                            .storage_controller
                            .collection(id)
                            .map_err(|_| DataflowCreationError::CollectionMissing(id))?
                            .collection_metadata
                            .clone();
                        let conn = PersistSinkConnection {
                            value_desc: conn.value_desc,
                            storage_metadata: metadata,
                        };
                        ComputeSinkConnection::Persist(conn)
                    }
                    ComputeSinkConnection::Subscribe(conn) => {
                        ComputeSinkConnection::Subscribe(conn)
                    }
                };
                let desc = ComputeSinkDesc {
                    from: se.from,
                    from_desc: se.from_desc,
                    connection,
                    as_of: se.as_of,
                };
                sink_exports.insert(id, desc);
            }

            augmented_dataflows.push(DataflowDescription {
                source_imports,
                sink_exports,
                // The rest of the fields are identical
                index_imports: d.index_imports,
                objects_to_build: d.objects_to_build,
                index_exports: d.index_exports,
                as_of: d.as_of,
                until: d.until,
                debug_name: d.debug_name,
            });
        }

        self.compute
            .send(ComputeCommand::CreateDataflows(augmented_dataflows));

        Ok(())
    }

    /// Drops the read capability for the given collections and allows their resources to be
    /// reclaimed.
    pub fn drop_collections(&mut self, ids: Vec<GlobalId>) -> Result<(), CollectionMissing> {
        // Validate that the ids exist.
        self.validate_ids(ids.iter().cloned())?;

        let policies = ids
            .into_iter()
            .map(|id| (id, ReadPolicy::ValidFrom(Antichain::new())));
        self.set_read_policy(policies.collect())
    }

    /// Initiate a peek request for the contents of `id` at `timestamp`.
    #[tracing::instrument(level = "debug", skip(self))]
    pub fn peek(
        &mut self,
        id: GlobalId,
        literal_constraints: Option<Vec<Row>>,
        uuid: Uuid,
        timestamp: T,
        finishing: RowSetFinishing,
        map_filter_project: mz_expr::SafeMfpPlan,
        target_replica: Option<ReplicaId>,
    ) -> Result<(), PeekError> {
        let since = self.compute.collection(id)?.read_capabilities.frontier();

        if !since.less_equal(&timestamp) {
            Err(PeekError::SinceViolation(id))?;
        }

        // Install a compaction hold on `id` at `timestamp`.
        let mut updates = BTreeMap::new();
        updates.insert(id, ChangeBatch::new_from(timestamp.clone(), 1));
        self.update_read_capabilities(&mut updates);

        let otel_ctx = OpenTelemetryContext::obtain();
        self.compute.peeks.insert(
            uuid,
            PendingPeek {
                target: id,
                time: timestamp.clone(),
                target_replica,
                // TODO(guswynn): can we just hold the `tracing::Span` here instead?
                otel_ctx: otel_ctx.clone(),
            },
        );

        self.compute.send(ComputeCommand::Peek(Peek {
            id,
            literal_constraints,
            uuid,
            timestamp,
            finishing,
            map_filter_project,
            // Obtain an `OpenTelemetryContext` from the thread-local tracing
            // tree to forward it on to the compute worker.
            otel_ctx,
        }));

        Ok(())
    }

    /// Cancels existing peek requests.
    pub fn cancel_peeks(&mut self, uuids: BTreeSet<Uuid>) {
        // Enqueue the response to the cancelation.
        for uuid in &uuids {
            let otel_ctx = self
                .compute
                .peeks
                .get_mut(uuid)
                .map(|pending| pending.otel_ctx.clone())
                .unwrap_or_else(|| {
                    tracing::warn!("did not find pending peek for {}", uuid);
                    OpenTelemetryContext::empty()
                });
            self.compute
                .ready_responses
                .push_back(ComputeControllerResponse::PeekResponse(
                    *uuid,
                    PeekResponse::Canceled,
                    otel_ctx,
                ));
        }

        // Canceled peeks should not be further responded to.
        self.remove_peeks(&uuids);

        self.compute.send(ComputeCommand::CancelPeeks { uuids });
    }

    /// Assigns a read policy to specific identifiers.
    ///
    /// The policies are assigned in the order presented, and repeated identifiers should
    /// conclude with the last policy. Changing a policy will immediately downgrade the read
    /// capability if appropriate, but it will not "recover" the read capability if the prior
    /// capability is already ahead of it.
    ///
    /// Identifiers not present in `policies` retain their existing read policies.
    #[tracing::instrument(level = "debug", skip(self))]
    pub fn set_read_policy(
        &mut self,
        policies: Vec<(GlobalId, ReadPolicy<T>)>,
    ) -> Result<(), CollectionMissing> {
        let mut read_capability_changes = BTreeMap::default();
        for (id, policy) in policies.into_iter() {
            let collection = self.compute.collection_mut(id)?;
            let mut new_read_capability = policy.frontier(collection.write_frontier.borrow());

            if timely::order::PartialOrder::less_equal(
                &collection.implied_capability,
                &new_read_capability,
            ) {
                let mut update = ChangeBatch::new();
                update.extend(new_read_capability.iter().map(|time| (time.clone(), 1)));
                std::mem::swap(&mut collection.implied_capability, &mut new_read_capability);
                update.extend(new_read_capability.iter().map(|time| (time.clone(), -1)));
                if !update.is_empty() {
                    read_capability_changes.insert(id, update);
                }
            }

            collection.read_policy = policy;
        }
        if !read_capability_changes.is_empty() {
            self.update_read_capabilities(&mut read_capability_changes);
        }
        Ok(())
    }

    /// Update the max size in bytes of any result.
    pub fn update_max_result_size(&mut self, max_result_size: u32) {
        self.compute
            .send(ComputeCommand::UpdateMaxResultSize(max_result_size))
    }

    /// Validate that a collection exists for all identifiers, and error if any do not.
    fn validate_ids(&self, ids: impl Iterator<Item = GlobalId>) -> Result<(), CollectionMissing> {
        for id in ids {
            self.compute.collection(id)?;
        }
        Ok(())
    }

    /// Accept write frontier updates from the compute layer.
    ///
    /// # Panics
    ///
    /// Panics if any of the `updates` references an absent collection.
    #[tracing::instrument(level = "debug", skip(self))]
    fn update_write_frontiers(
        &mut self,
        replica_id: ReplicaId,
        updates: &[(GlobalId, Antichain<T>)],
    ) {
        let mut advanced_collections = Vec::new();
        let mut compute_read_capability_changes = BTreeMap::default();
        let mut storage_read_capability_changes = BTreeMap::default();
        let mut dropped_collection_ids = Vec::new();
        for (id, new_upper) in updates.iter() {
            let collection = self
                .compute
                .collection_mut(*id)
                .expect("reference to absent collection");

            if PartialOrder::less_than(&collection.write_frontier, new_upper) {
                advanced_collections.push(*id);
                collection.write_frontier = new_upper.clone();
            }

            let old_upper = collection
                .replica_write_frontiers
                .insert(replica_id, new_upper.clone());

            if new_upper.is_empty() {
                dropped_collection_ids.push(*id);
            }

            let mut new_read_capability = collection
                .read_policy
                .frontier(collection.write_frontier.borrow());
            if timely::order::PartialOrder::less_equal(
                &collection.implied_capability,
                &new_read_capability,
            ) {
                let mut update = ChangeBatch::new();
                update.extend(new_read_capability.iter().map(|time| (time.clone(), 1)));
                std::mem::swap(&mut collection.implied_capability, &mut new_read_capability);
                update.extend(new_read_capability.iter().map(|time| (time.clone(), -1)));
                if !update.is_empty() {
                    compute_read_capability_changes.insert(*id, update);
                }
            }

            // Update read holds on storage dependencies.
            for storage_id in &collection.storage_dependencies {
                let update = storage_read_capability_changes
                    .entry(*storage_id)
                    .or_insert_with(|| ChangeBatch::new());
                if let Some(old) = &old_upper {
                    update.extend(old.iter().map(|time| (time.clone(), -1)));
                }
                update.extend(new_upper.iter().map(|time| (time.clone(), 1)));
            }
        }
        if !compute_read_capability_changes.is_empty() {
            self.update_read_capabilities(&mut compute_read_capability_changes);
        }
        if !storage_read_capability_changes.is_empty() {
            self.storage_controller
                .update_read_capabilities(&mut storage_read_capability_changes);
        }

        // Tell the storage controller about new write frontiers for storage
        // collections that are advanced by compute sinks.
        // TODO(teskje): The storage controller should have a task to directly
        // keep track of the frontiers of storage collections, instead of
        // relying on others for that information.
        let storage_updates: Vec<_> = advanced_collections
            .into_iter()
            .filter(|id| self.storage_controller.collection(*id).is_ok())
            .map(|id| {
                let collection = self.compute.collection(id).unwrap();
                (id, collection.write_frontier.clone())
            })
            .collect();
        self.storage_controller
            .update_write_frontiers(&storage_updates);

        if !dropped_collection_ids.is_empty() {
            self.update_dropped_collections(dropped_collection_ids);
        }
    }

    /// Remove frontier tracking state for the given replica.
    #[tracing::instrument(level = "debug", skip(self))]
    fn remove_write_frontiers(&mut self, replica_id: ReplicaId) {
        let mut storage_read_capability_changes = BTreeMap::default();
        let mut dropped_collection_ids = Vec::new();
        for (id, collection) in self.compute.collections.iter_mut() {
            let last_upper = collection.replica_write_frontiers.remove(&replica_id);

            if let Some(frontier) = last_upper {
                dropped_collection_ids.push(*id);

                // Update read holds on storage dependencies.
                for storage_id in &collection.storage_dependencies {
                    let update = storage_read_capability_changes
                        .entry(*storage_id)
                        .or_insert_with(|| ChangeBatch::new());
                    update.extend(frontier.iter().map(|time| (time.clone(), -1)));
                }
            }
        }
        if !storage_read_capability_changes.is_empty() {
            self.storage_controller
                .update_read_capabilities(&mut storage_read_capability_changes);
        }
        if !dropped_collection_ids.is_empty() {
            self.update_dropped_collections(dropped_collection_ids);
        }
    }

    /// Applies `updates`, propagates consequences through other read capabilities, and sends an appropriate compaction command.
    #[tracing::instrument(level = "debug", skip(self))]
    fn update_read_capabilities(&mut self, updates: &mut BTreeMap<GlobalId, ChangeBatch<T>>) {
        // Locations to record consequences that we need to act on.
        let mut storage_todo = BTreeMap::default();
        let mut compute_net = Vec::default();
        // Repeatedly extract the maximum id, and updates for it.
        while let Some(key) = updates.keys().rev().next().cloned() {
            let mut update = updates.remove(&key).unwrap();
            if let Ok(collection) = self.compute.collection_mut(key) {
                let changes = collection.read_capabilities.update_iter(update.drain());
                update.extend(changes);
                for id in collection.storage_dependencies.iter() {
                    storage_todo
                        .entry(*id)
                        .or_insert_with(ChangeBatch::new)
                        .extend(update.iter().cloned());
                }
                for id in collection.compute_dependencies.iter() {
                    updates
                        .entry(*id)
                        .or_insert_with(ChangeBatch::new)
                        .extend(update.iter().cloned());
                }
                compute_net.push((key, update));
            } else {
                // Storage presumably, but verify.
                if self.storage_controller.collection(key).is_ok() {
                    storage_todo
                        .entry(key)
                        .or_insert_with(ChangeBatch::new)
                        .extend(update.drain())
                } else {
                    tracing::error!(
                        "found neither compute nor storage collection with id {}",
                        key
                    );
                }
            }
        }

        // Translate our net compute actions into `AllowCompaction` commands
        // and a list of collections that are potentially ready to be dropped
        let mut compaction_commands = Vec::new();
        let mut dropped_collection_ids = Vec::new();
        for (id, change) in compute_net.iter_mut() {
            let frontier = self
                .compute
                .collection(*id)
                .expect("existence checked above")
                .read_frontier();
            if frontier.is_empty() {
                dropped_collection_ids.push(*id);
            }
            if !change.is_empty() {
                let frontier = frontier.to_owned();
                compaction_commands.push((*id, frontier));
            }
        }
        if !compaction_commands.is_empty() {
            self.compute
                .send(ComputeCommand::AllowCompaction(compaction_commands));
        }
        if !dropped_collection_ids.is_empty() {
            self.update_dropped_collections(dropped_collection_ids);
        }

        // We may have storage consequences to process.
        if !storage_todo.is_empty() {
            self.storage_controller
                .update_read_capabilities(&mut storage_todo);
        }
    }

    /// Removes a registered peek, unblocking compaction that might have waited on it.
    fn remove_peeks(&mut self, peek_ids: &BTreeSet<Uuid>) {
        let mut updates = peek_ids
            .into_iter()
            .flat_map(|uuid| {
                self.compute
                    .peeks
                    .remove(uuid)
                    .map(|peek| (peek.target, ChangeBatch::new_from(peek.time, -1)))
            })
            .collect();
        self.update_read_capabilities(&mut updates);
    }

    pub fn handle_response(
        &mut self,
        response: ComputeResponse<T>,
        replica_id: ReplicaId,
    ) -> Option<ComputeControllerResponse<T>> {
        match response {
            ComputeResponse::FrontierUppers(list) => {
                self.handle_frontier_uppers(list, replica_id);
                None
            }
            ComputeResponse::PeekResponse(uuid, peek_response, otel_ctx) => {
                self.handle_peek_response(uuid, peek_response, otel_ctx, replica_id)
            }
            ComputeResponse::SubscribeResponse(id, response) => {
                self.handle_subscribe_response(id, response, replica_id)
            }
        }
    }

    /// Cleans up collection state, if necessary, in response to drop operations targeted
    /// at a replica and given collections (via reporting of an empty frontier).
    fn update_dropped_collections(&mut self, dropped_collection_ids: Vec<GlobalId>) {
        for id in dropped_collection_ids {
            // clean up the given collection if read frontier is empty
            // and all replica frontiers are empty
            if let Ok(collection) = self.compute.collection(id) {
                if collection.read_frontier().is_empty()
                    && collection
                        .replica_write_frontiers
                        .values()
                        .all(|frontier| frontier.is_empty())
                {
                    self.compute.collections.remove(&id);
                }
            }
        }
    }

    fn handle_frontier_uppers(
        &mut self,
        list: Vec<(GlobalId, Antichain<T>)>,
        replica_id: ReplicaId,
    ) {
        // We should not receive updates for collections we don't track. It is plausible that we
        // currently do due to a bug where replicas send `FrontierUppers` for collections they drop
        // during reconciliation.
        // TODO(teskje): Revisit this after #15535 is resolved.
        let updates: Vec<_> = list
            .into_iter()
            .filter(|(id, _)| self.compute.collections.contains_key(id))
            .collect();

        self.update_write_frontiers(replica_id, &updates);
    }

    fn handle_peek_response(
        &mut self,
        uuid: Uuid,
        response: PeekResponse,
        otel_ctx: OpenTelemetryContext,
        replica_id: ReplicaId,
    ) -> Option<ComputeControllerResponse<T>> {
        // Forward the peek response, if we didn't already forward a response
        // to this peek previously. If the peek is targeting a replica, only
        // forward the response from that replica.

        let peek = self.compute.peeks.get(&uuid)?;

        let target_replica = peek.target_replica.unwrap_or(replica_id);
        if target_replica != replica_id {
            return None;
        }

        self.remove_peeks(&[uuid].into());

        // NOTE: we use the `otel_ctx` from the response, not the
        // pending peek, because we currently want the parent
        // to be whatever the compute worker did with this peek.
        //
        // Additionally, we just use the `otel_ctx` from the first worker to
        // respond.
        Some(ComputeControllerResponse::PeekResponse(
            uuid, response, otel_ctx,
        ))
    }

    fn handle_subscribe_response(
        &mut self,
        subscribe_id: GlobalId,
        response: SubscribeResponse<T>,
        replica_id: ReplicaId,
    ) -> Option<ComputeControllerResponse<T>> {
        let mut frontier_updates = Vec::new();
        let controller_response = match response {
            SubscribeResponse::Batch(SubscribeBatch {
                lower: _,
                upper,
                mut updates,
            }) => {
                frontier_updates.push((subscribe_id, upper.clone()));

                // If this batch advances the subscribe's frontier, we emit all updates at times
                // greater or equal to the last frontier (to avoid emitting duplicate updates).
                // let old_upper_bound = entry.bounds.upper.clone();
                let prev_frontier = self
                    .compute
                    .subscribes
                    .remove(&subscribe_id)
                    .unwrap_or_else(Antichain::new);

                if PartialOrder::less_than(&prev_frontier, &upper) {
                    if !upper.is_empty() {
                        // This subscribe can produce more data. Keep tracking it.
                        self.compute.subscribes.insert(subscribe_id, upper.clone());
                    }

                    if let Ok(updates) = updates.as_mut() {
                        updates.retain(|(time, _data, _diff)| prev_frontier.less_equal(time));
                    }
                    Some(ComputeControllerResponse::SubscribeResponse(
                        subscribe_id,
                        SubscribeResponse::Batch(SubscribeBatch {
                            lower: prev_frontier,
                            upper,
                            updates,
                        }),
                    ))
                } else {
                    if !prev_frontier.is_empty() {
                        // This subscribe can produce more data. Keep tracking it.
                        self.compute.subscribes.insert(subscribe_id, prev_frontier);
                    }
                    None
                }
            }
            SubscribeResponse::DroppedAt(_) => {
                frontier_updates.push((subscribe_id, Antichain::new()));

                // If this subscribe is still in progress, forward the `DroppedAt` response.
                // Otherwise ignore it.
                if let Some(frontier) = self.compute.subscribes.remove(&subscribe_id) {
                    Some(ComputeControllerResponse::SubscribeResponse(
                        subscribe_id,
                        SubscribeResponse::DroppedAt(frontier),
                    ))
                } else {
                    None
                }
            }
        };

        self.update_write_frontiers(replica_id, &frontier_updates);
        controller_response
    }
}

#[derive(Debug)]
struct PendingPeek<T> {
    /// ID of the collection targeted by this peek.
    target: GlobalId,
    /// The peek time.
    time: T,
    /// For replica-targeted peeks, this specifies the replica whose response we should pass on.
    ///
    /// If this value is `None`, we pass on the first response.
    target_replica: Option<ReplicaId>,
    /// The OpenTelemetry context for this peek.
    otel_ctx: OpenTelemetryContext,
}
