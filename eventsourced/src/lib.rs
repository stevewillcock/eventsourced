//! Event sourced entities.

#![allow(clippy::type_complexity)]
#![allow(incomplete_features)]
#![feature(async_fn_in_trait)]
#![feature(return_position_impl_trait_in_trait)]
#![feature(type_alias_impl_trait)]

pub mod convert;
mod evt_log;
mod snapshot_store;

pub use evt_log::*;
pub use snapshot_store::*;

use bytes::Bytes;
use futures::StreamExt;
use std::{
    any::Any,
    error::Error as StdError,
    fmt::Debug,
    num::{NonZeroU64, NonZeroUsize},
};
use thiserror::Error;
use tokio::{
    pin,
    sync::{mpsc, oneshot},
    task,
};
use tracing::{debug, error};
use uuid::Uuid;

/// Optional metadata to optimize sequence number based lookup of events in the [EvtLog].
pub type Metadata = Option<Box<dyn Any + Send>>;

/// Command and event handling for an event sourced entity.
pub trait EventSourced: Send + Sized + 'static {
    /// Command type.
    type Cmd: Debug + Send + Sync + 'static;

    /// Event type.
    type Evt: Debug + Send + Sync + 'static;

    /// Snapshot state type.
    type State: Send + Sync;

    /// Error type for rejected (a.k.a. invalid) commands.
    type Error: StdError + Send + Sync + 'static;

    /// Command handler, returning the to be persisted events or an error.
    fn handle_cmd(&self, cmd: Self::Cmd) -> Result<Vec<Self::Evt>, Self::Error>;

    /// Event handler, returning whether to take a snapshot or not.
    fn handle_evt(&mut self, seq_no: u64, evt: &Self::Evt) -> Option<Self::State>;

    /// Snapshot state handler.
    fn set_state(&mut self, state: Self::State);
}

/// Extension methods for types implementing [EventSourced].
pub trait EventSourcedExt {
    /// Spawns an entity implementing [EventSourced] with the given ID and creates an [EntityRef]
    /// as a handle for it.
    ///
    /// First the given [SnapshotStore] is used to find and possibly load a snapshot. Then the
    /// [EvtLog] is used to find the last sequence number and then to load any remaining events.
    ///
    /// Commands can be passed to the spawned entity by invoking `handle_cmd` on the returned
    /// [EntityRef] which uses a buffered channel with the given size.
    ///
    /// Commands are handled by the command handler of this entity. They can be rejected by
    /// returning an error. Valid commands may produce events which get persisted to the [EvtLog]
    /// along with an increasing sequence number and then applied to the event handler of this
    /// entity. The event handler may decide to save a snapshot at the current sequence number
    /// which is used to speed up future spawning.
    async fn spawn<
        L,
        S,
        EvtToBytes,
        EvtToBytesError,
        StateToBytes,
        StateToBytesError,
        EvtFromBytes,
        EvtFromBytesError,
        StateFromBytes,
        StateFromBytesError,
    >(
        mut self,
        id: Uuid,
        buffer: NonZeroUsize,
        evt_log: L,
        snapshot_store: S,
        binarizer: Binarizer<EvtToBytes, EvtFromBytes, StateToBytes, StateFromBytes>,
    ) -> Result<EntityRef<Self>, SpawnError>
    where
        Self: EventSourced,
        L: EvtLog,
        S: SnapshotStore,
        EvtToBytes: Fn(&Self::Evt) -> Result<Bytes, EvtToBytesError> + Send + Sync + 'static,
        EvtToBytesError: StdError + Send + Sync + 'static,
        StateToBytes: Fn(&Self::State) -> Result<Bytes, StateToBytesError> + Send + Sync + 'static,
        StateToBytesError: StdError + Send + Sync + 'static,
        EvtFromBytes:
            Fn(Bytes) -> Result<Self::Evt, EvtFromBytesError> + Copy + Send + Sync + 'static,
        EvtFromBytesError: StdError + Send + Sync + 'static,
        StateFromBytes:
            Fn(Bytes) -> Result<Self::State, StateFromBytesError> + Copy + Send + Sync + 'static,
        StateFromBytesError: StdError + Send + Sync + 'static,
    {
        let Binarizer {
            evt_to_bytes,
            evt_from_bytes,
            state_to_bytes,
            state_from_bytes,
        } = binarizer;

        // Restore snapshot.
        let (snapshot_seq_no, metadata) = snapshot_store
            .load::<Self::State, _, _>(id, state_from_bytes)
            .await
            .map_err(|source| SpawnError::LoadSnapshot(source.into()))?
            .map(
                |Snapshot {
                     seq_no,
                     state,
                     metadata,
                 }| {
                    debug!(%id, seq_no, "Restoring snapshot");
                    self.set_state(state);
                    (seq_no, metadata)
                },
            )
            .unwrap_or((0, None));

        // Replay latest events.
        let last_seq_no = evt_log
            .last_seq_no(id)
            .await
            .map_err(|source| SpawnError::LastSeqNo(source.into()))?;
        assert!(
            snapshot_seq_no <= last_seq_no,
            "snapshot_seq_no must be less than or equal to last_seq_no"
        );
        if snapshot_seq_no < last_seq_no {
            let from_seq_no = unsafe { NonZeroU64::new_unchecked(snapshot_seq_no + 1) };
            debug!(%id, from_seq_no, last_seq_no , "Replaying evts");
            let evts = evt_log
                .evts_by_id::<Self::Evt, _, _>(
                    id,
                    from_seq_no,
                    last_seq_no,
                    metadata,
                    evt_from_bytes,
                )
                .await
                .map_err(|source| SpawnError::EvtsById(source.into()))?;
            pin!(evts);
            while let Some(evt) = evts.next().await {
                let (seq_no, evt) = evt.map_err(|source| SpawnError::NextEvt(source.into()))?;
                self.handle_evt(seq_no, &evt);
            }
        }

        // Create entity.
        let mut entity = Entity {
            event_sourced: self,
            id,
            last_seq_no,
            evt_log,
            snapshot_store,
            evt_to_bytes,
            state_to_bytes,
        };
        debug!(%id, "EventSourced entity created");

        let (cmd_in, mut cmd_out) = mpsc::channel::<(
            Self::Cmd,
            oneshot::Sender<Result<Vec<Self::Evt>, Self::Error>>,
        )>(buffer.get());

        // Spawn handler loop.
        task::spawn(async move {
            while let Some((cmd, result_sender)) = cmd_out.recv().await {
                match entity.handle_cmd(cmd).await {
                    Ok(result) => {
                        if result_sender.send(result).is_err() {
                            error!(%id, "Cannot send command handler result");
                        };
                    }
                    Err(error) => {
                        error!(%id, %error, "Cannot persist events");
                        break;
                    }
                }
            }
            debug!(%id, "Eventsourced entity terminated");
        });

        Ok(EntityRef { id, cmd_in })
    }
}

impl<E> EventSourcedExt for E where E: EventSourced {}

struct Entity<E, L, S, EvtToBytes, StateToBytes> {
    event_sourced: E,
    id: Uuid,
    last_seq_no: u64,
    evt_log: L,
    snapshot_store: S,
    evt_to_bytes: EvtToBytes,
    state_to_bytes: StateToBytes,
}

impl<E, L, S, EvtToBytes, EvtToBytesError, StateToBytes, StateToBytesError>
    Entity<E, L, S, EvtToBytes, StateToBytes>
where
    E: EventSourced,
    L: EvtLog,
    S: SnapshotStore,
    EvtToBytes: Fn(&E::Evt) -> Result<Bytes, EvtToBytesError> + Send + Sync + 'static,
    EvtToBytesError: StdError + Send + Sync + 'static,
    StateToBytes: Fn(&E::State) -> Result<Bytes, StateToBytesError> + Send + Sync + 'static,
    StateToBytesError: StdError + Send + Sync + 'static,
{
    async fn handle_cmd(
        &mut self,
        cmd: E::Cmd,
    ) -> Result<Result<Vec<E::Evt>, E::Error>, Box<dyn StdError>> {
        // Handle command.
        let evts = match self.event_sourced.handle_cmd(cmd) {
            Ok(evts) => evts,
            Err(error) => return Ok(Err(error)),
        };

        if !evts.is_empty() {
            // Persist events.
            let metadata = self
                .evt_log
                .persist(self.id, &evts, self.last_seq_no, &self.evt_to_bytes)
                .await?;

            // Handle persisted events.
            let state = evts.iter().fold(None, |state, evt| {
                self.last_seq_no += 1;
                self.event_sourced
                    .handle_evt(self.last_seq_no, evt)
                    .or(state)
            });

            // Persist latest snapshot if any.
            if let Some(state) = state {
                debug!(id = %self.id, seq_no = self.last_seq_no, "Saving snapshot");
                self.snapshot_store
                    .save(
                        self.id,
                        unsafe { NonZeroU64::new_unchecked(self.last_seq_no) },
                        state,
                        metadata,
                        &self.state_to_bytes,
                    )
                    .await?;
            }
        }

        Ok(Ok(evts))
    }
}

/// Errors from spawning an event sourced entity.
#[derive(Debug, Error)]
pub enum SpawnError {
    /// A snapshot cannot be loaded from the snapshot store.
    #[error("Cannot load snapshot from snapshot store")]
    LoadSnapshot(#[source] Box<dyn StdError + Send + Sync>),

    /// The last seqence number cannot be obtained from the event log.
    #[error("Cannot get last seqence number from event log")]
    LastSeqNo(#[source] Box<dyn StdError + Send + Sync>),

    /// Events by ID cannot be obtained from the event log.
    #[error("Cannot get events by ID from event log")]
    EvtsById(#[source] Box<dyn StdError + Send + Sync>),

    /// The next event cannot be obtained from the event log.
    #[error("Cannot get next event from event log")]
    NextEvt(#[source] Box<dyn StdError + Send + Sync>),
}

/// A handle for a spawned [EventSourced] entity which can be used to invoke its command handler.
#[derive(Debug, Clone)]
pub struct EntityRef<E>
where
    E: EventSourced,
{
    id: Uuid,
    cmd_in: mpsc::Sender<(E::Cmd, oneshot::Sender<Result<Vec<E::Evt>, E::Error>>)>,
}

impl<E> EntityRef<E>
where
    E: EventSourced,
{
    /// Get the ID of the proxied event sourced entity.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Invoke the command handler of the entity.
    ///
    /// The returned (outer) `Result` signals, whether the command could be sent to the entity and
    /// the command handler result could be received, i.e. an `Err` signals a technical failure.
    ///
    /// The (outer) `Ok` variant contains another (inner) `Result`, which signals whether the
    /// command was valid or rejected. If it was valid, the persisted events are returned, else the
    /// rejection error.
    pub async fn handle_cmd(
        &self,
        cmd: E::Cmd,
    ) -> Result<Result<Vec<E::Evt>, E::Error>, EntityRefError> {
        let (result_in, result_out) = oneshot::channel();
        self.cmd_in
            .send((cmd, result_in))
            .await
            .map_err(|source| EntityRefError::SendCmd(source.into()))?;
        result_out.await.map_err(EntityRefError::RcvHandlerResult)
    }
}

/// Errors from an [EntityRef].
#[derive(Debug, Error)]
pub enum EntityRefError {
    /// A command cannot be sent from an [EntityRef] to its entity.
    #[error("Cannot send command to Entity")]
    SendCmd(#[source] Box<dyn StdError + Send + Sync>),

    /// An [EntityRef] cannot receive the command handler result from its entity, potentially
    /// because its entity has terminated.
    #[error("Cannot receive command handler result from Entity")]
    RcvHandlerResult(#[from] oneshot::error::RecvError),
}

/// Collection of conversion functions from and to [Bytes](bytes::Bytes) for events and snapshots.
pub struct Binarizer<EvtToBytes, EvtFromBytes, StateToBytes, StateFromBytes> {
    pub evt_to_bytes: EvtToBytes,
    pub evt_from_bytes: EvtFromBytes,
    pub state_to_bytes: StateToBytes,
    pub state_from_bytes: StateFromBytes,
}

#[cfg(all(test, feature = "prost"))]
mod tests {
    use super::*;
    use async_stream::stream;
    use bytes::BytesMut;
    use futures::Stream;
    use prost::Message;
    use std::convert::Infallible;

    #[derive(Debug)]
    struct Simple(u64);

    impl EventSourced for Simple {
        type Cmd = ();

        type Evt = u64;

        type State = u64;

        type Error = Infallible;

        fn handle_cmd(&self, _cmd: Self::Cmd) -> Result<Vec<Self::Evt>, Self::Error> {
            Ok(vec![
                (1 << 32) + self.0,
                (2 << 32) + self.0,
                (3 << 32) + self.0,
            ])
        }

        fn handle_evt(&mut self, _seq_no: u64, evt: &Self::Evt) -> Option<Self::State> {
            self.0 += evt >> 32;
            None
        }

        fn set_state(&mut self, state: Self::State) {
            self.0 = state;
        }
    }

    #[derive(Debug, Clone)]
    struct TestEvtLog;

    impl EvtLog for TestEvtLog {
        type Error = TestEvtLogError;

        async fn persist<'a, E, ToBytes, ToBytesError>(
            &'a mut self,
            _id: Uuid,
            _evts: &'a [E],
            _last_seq_no: u64,
            _to_bytes: &'a ToBytes,
        ) -> Result<Metadata, Self::Error>
        where
            E: Send + Sync + 'a,
            ToBytes: Fn(&E) -> Result<Bytes, ToBytesError> + Send + Sync,
            ToBytesError: StdError + Send + Sync + 'static,
        {
            Ok(None)
        }

        async fn last_seq_no(&self, _entity_id: Uuid) -> Result<u64, Self::Error> {
            Ok(42)
        }

        async fn evts_by_id<'a, E, EvtFromBytes, EvtFromBytesError>(
            &'a self,
            _id: Uuid,
            from_seq_no: NonZeroU64,
            to_seq_no: u64,
            _metadata: Metadata,
            evt_from_bytes: EvtFromBytes,
        ) -> Result<impl Stream<Item = Result<(u64, E), Self::Error>> + Send, Self::Error>
        where
            E: Send + 'a,
            EvtFromBytes: Fn(Bytes) -> Result<E, EvtFromBytesError> + Copy + Send + Sync + 'static,
            EvtFromBytesError: StdError + Send + Sync + 'static,
        {
            let evts = stream! {
                for n in 0..666 {
                    for evt in 1..=3 {
                        let seq_no = n * 3 + evt;
                        if from_seq_no.get() <= seq_no && seq_no <= to_seq_no {
                            let mut bytes = BytesMut::new();
                            evt.encode(&mut bytes).map_err(|source| TestEvtLogError(source.into()))?;
                            let evt = evt_from_bytes(bytes.into()).map_err(|source| TestEvtLogError(source.into()))?;
                            yield Ok((seq_no, evt));
                        }
                    }

                }
            };
            Ok(evts)
        }

        async fn ids(&self) -> Result<impl Stream<Item = Uuid>, Self::Error> {
            Ok(futures::stream::empty())
        }
    }

    #[derive(Debug, Error)]
    #[error("TestEvtLogError")]
    struct TestEvtLogError(#[source] Box<dyn StdError + Send + Sync>);

    #[derive(Debug, Clone)]
    struct TestSnapshotStore;

    impl SnapshotStore for TestSnapshotStore {
        type Error = TestSnapshotStoreError;

        async fn save<'a, S, StateToBytes, StateToBytesError>(
            &'a mut self,
            _id: Uuid,
            _seq_no: NonZeroU64,
            _state: S,
            _metadata: Metadata,
            _state_to_bytes: &'a StateToBytes,
        ) -> Result<(), Self::Error>
        where
            S: Send + Sync + 'a,
            StateToBytes: Fn(&S) -> Result<Bytes, StateToBytesError> + Send + Sync + 'static,
            StateToBytesError: StdError + Send + Sync + 'static,
        {
            Ok(())
        }

        async fn load<'a, S, StateFromBytes, StateFromBytesError>(
            &'a self,
            _id: Uuid,
            state_from_bytes: StateFromBytes,
        ) -> Result<Option<Snapshot<S>>, Self::Error>
        where
            S: 'a,
            StateFromBytes:
                Fn(Bytes) -> Result<S, StateFromBytesError> + Copy + Send + Sync + 'static,
            StateFromBytesError: StdError + Send + Sync + 'static,
        {
            let mut bytes = BytesMut::new();
            42.encode(&mut bytes).unwrap();
            let state = state_from_bytes(bytes.into()).unwrap();
            Ok(Some(Snapshot {
                seq_no: 42,
                state,
                metadata: None,
            }))
        }
    }

    #[derive(Debug, Error)]
    #[error("TestSnapshotStoreError")]
    struct TestSnapshotStoreError;

    #[tokio::test]
    async fn test_spawn_handle_cmd() -> Result<(), Box<dyn StdError>> {
        let evt_log = TestEvtLog;
        let snapshot_store = TestSnapshotStore;

        let entity = spawn(evt_log, snapshot_store).await?;

        let evts = entity.handle_cmd(()).await??;
        assert_eq!(evts, vec![(1 << 32) + 42, (2 << 32) + 42, (3 << 32) + 42]);

        Ok(())
    }

    // We go through these hoops to ensure oddities in "async fn in trait" and other unstable
    // features are handle appropriately, e.g. by asserting futures are send.
    async fn spawn<E, S>(
        evt_log: E,
        snapshot_store: S,
    ) -> Result<EntityRef<Simple>, Box<dyn StdError>>
    where
        E: EvtLog,
        S: SnapshotStore,
    {
        let entity = task::spawn(async move {
            Simple(0).spawn(
                Uuid::now_v7(),
                unsafe { NonZeroUsize::new_unchecked(1) },
                evt_log,
                snapshot_store,
                convert::prost::binarizer(),
            )
        });

        let entity = entity.await?.await?;
        Ok(entity)
    }
}
