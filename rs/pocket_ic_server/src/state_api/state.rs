/// This module contains the core state of the PocketIc server.
/// Axum handlers operate on a global state of type ApiState, whose
/// interface guarantees consistency and determinism.
use crate::pocket_ic::{PocketIc, SetTimeAndTick};
use crate::InstanceId;
use crate::{OpId, Operation};
use base64;
use ic_types::{CanisterId, SubnetId};
use ic_utils::thread::JoinOnDrop;
use pocket_ic::{ErrorCode, UserError, WasmResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    thread::Builder as ThreadBuilder,
    time::{Duration, SystemTime},
};
use tokio::{
    sync::mpsc::error::TryRecvError,
    sync::{mpsc, Mutex, RwLock},
    task::{spawn, spawn_blocking, JoinHandle},
    time::{self, sleep},
};
use tracing::trace;

// The maximum wait time for a computation to finish synchronously.
const DEFAULT_SYNC_WAIT_DURATION: Duration = Duration::from_secs(10);

// The minimum delay between consecutive ticks in auto progress mode.
const MIN_TICK_DELAY: Duration = Duration::from_millis(100);

pub const STATE_LABEL_HASH_SIZE: usize = 32;

/// Uniquely identifies a state.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct StateLabel(pub [u8; STATE_LABEL_HASH_SIZE]);

// The only error condition is if the vector has the wrong size.
pub struct InvalidSize;

impl std::fmt::Debug for StateLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StateLabel(")?;
        self.0.iter().try_for_each(|b| write!(f, "{:02X}", b))?;
        write!(f, ")")
    }
}

impl std::convert::TryFrom<Vec<u8>> for StateLabel {
    // The input vector having the wrong size is the only possible error condition.
    type Error = InvalidSize;

    fn try_from(v: Vec<u8>) -> Result<StateLabel, InvalidSize> {
        if v.len() != STATE_LABEL_HASH_SIZE {
            return Err(InvalidSize);
        }

        let mut res = StateLabel::default();
        res.0[0..STATE_LABEL_HASH_SIZE].clone_from_slice(v.as_slice());
        Ok(res)
    }
}

struct ProgressThread {
    handle: JoinHandle<()>,
    sender: mpsc::Sender<()>,
}

/// The state of the PocketIC API.
pub struct ApiState {
    // impl note: If locks are acquired on both fields, acquire first on instances, then on graph.
    instances: Arc<RwLock<Vec<Mutex<InstanceState>>>>,
    graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
    // threads making IC instances progress automatically
    progress_threads: RwLock<Vec<Mutex<Option<ProgressThread>>>>,
    sync_wait_time: Duration,
    // dropping the PocketIC instance might be an expensive operation (the state machine is
    // deallocated, e.g.). Thus, we immediately mark the instance as deleted while sending the
    // PocketIC instance to a background worker and drop it there.
    drop_sender: mpsc::UnboundedSender<PocketIc>,
    _drop_worker_handle: JoinOnDrop<()>,
}

#[derive(Default)]
pub struct PocketIcApiStateBuilder {
    initial_instances: Vec<PocketIc>,
    sync_wait_time: Option<Duration>,
}

impl PocketIcApiStateBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    /// Computations are dispatched into background tasks. If a computation takes longer than
    /// [sync_wait_time], the update-operation returns, indicating that the given instance is busy.
    pub fn with_sync_wait_time(self, sync_wait_time: Duration) -> Self {
        Self {
            sync_wait_time: Some(sync_wait_time),
            ..self
        }
    }

    /// Will make the given instance available in the initial state.
    pub fn add_initial_instance(mut self, instance: PocketIc) -> Self {
        self.initial_instances.push(instance);
        self
    }

    pub fn build(self) -> Arc<ApiState> {
        let graph: HashMap<StateLabel, Computations> = self
            .initial_instances
            .iter()
            .map(|i| (i.get_state_label(), Computations::default()))
            .collect();
        let graph = RwLock::new(graph);

        let instances: Vec<_> = self
            .initial_instances
            .into_iter()
            .map(|inst| Mutex::new(InstanceState::Available(inst)))
            .collect();
        let instances_len = instances.len();
        let instances = RwLock::new(instances);

        let progress_threads = RwLock::new((0..instances_len).map(|_| Mutex::new(None)).collect());

        let sync_wait_time = self.sync_wait_time.unwrap_or(DEFAULT_SYNC_WAIT_DURATION);
        #[allow(clippy::disallowed_methods)]
        let (drop_sender, mut rx) = mpsc::unbounded_channel::<PocketIc>();
        let drop_handle = ThreadBuilder::new()
            .name("PocketIC GC Thread".into())
            .spawn(move || {
                while let Some(pocket_ic) = rx.blocking_recv() {
                    std::mem::drop(pocket_ic);
                }
            })
            .unwrap();

        Arc::new(ApiState {
            instances: instances.into(),
            graph: graph.into(),
            progress_threads,
            sync_wait_time,
            drop_sender,
            _drop_worker_handle: JoinOnDrop::new(drop_handle),
        })
    }
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum OpOut {
    NoOutput,
    Time(u64),
    CanisterResult(Result<WasmResult, UserError>),
    CanisterId(CanisterId),
    Cycles(u128),
    Bytes(Vec<u8>),
    SubnetId(SubnetId),
    Error(PocketIcError),
    ApiV2Response((u16, BTreeMap<String, Vec<u8>>, Vec<u8>)),
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum PocketIcError {
    CanisterNotFound(CanisterId),
    BadIngressMessage(String),
    SubnetNotFound(candid::Principal),
    RequestRoutingError(String),
}

impl From<Result<ic_state_machine_tests::WasmResult, ic_state_machine_tests::UserError>> for OpOut {
    fn from(
        r: Result<ic_state_machine_tests::WasmResult, ic_state_machine_tests::UserError>,
    ) -> Self {
        let res = {
            match r {
                Ok(ic_state_machine_tests::WasmResult::Reply(wasm)) => Ok(WasmResult::Reply(wasm)),
                Ok(ic_state_machine_tests::WasmResult::Reject(s)) => Ok(WasmResult::Reject(s)),
                Err(user_err) => Err(UserError {
                    code: ErrorCode::try_from(user_err.code() as u64).unwrap(),
                    description: user_err.description().to_string(),
                }),
            }
        };
        OpOut::CanisterResult(res)
    }
}

// TODO: Remove this Into: It's only used in the InstallCanisterAsController Operation, which also should be removed.
impl From<Result<(), ic_state_machine_tests::UserError>> for OpOut {
    fn from(r: Result<(), ic_state_machine_tests::UserError>) -> Self {
        let res = {
            match r {
                Ok(_) => Ok(WasmResult::Reply(vec![])),
                Err(user_err) => Err(UserError {
                    code: ErrorCode::try_from(user_err.code() as u64).unwrap(),
                    description: user_err.description().to_string(),
                }),
            }
        };
        OpOut::CanisterResult(res)
    }
}

impl std::fmt::Debug for OpOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpOut::NoOutput => write!(f, "NoOutput"),
            OpOut::Time(x) => write!(f, "Time({})", x),
            OpOut::CanisterId(cid) => write!(f, "CanisterId({})", cid),
            OpOut::Cycles(x) => write!(f, "Cycles({})", x),
            OpOut::CanisterResult(Ok(x)) => write!(f, "CanisterResult: Ok({:?})", x),
            OpOut::CanisterResult(Err(x)) => write!(f, "CanisterResult: Err({})", x),
            OpOut::Error(PocketIcError::CanisterNotFound(cid)) => {
                write!(f, "CanisterNotFound({})", cid)
            }
            OpOut::Error(PocketIcError::BadIngressMessage(msg)) => {
                write!(f, "BadIngressMessage({})", msg)
            }
            OpOut::Error(PocketIcError::SubnetNotFound(sid)) => {
                write!(f, "SubnetNotFound({})", sid)
            }
            OpOut::Error(PocketIcError::RequestRoutingError(msg)) => {
                write!(f, "RequestRoutingError({:?})", msg)
            }
            OpOut::Bytes(bytes) => write!(f, "Bytes({})", base64::encode(bytes)),
            OpOut::SubnetId(subnet_id) => write!(f, "SubnetId({})", subnet_id),
            OpOut::ApiV2Response((status, headers, bytes)) => {
                write!(
                    f,
                    "ApiV2Resp({}:{:?}:{})",
                    status,
                    headers,
                    base64::encode(bytes)
                )
            }
        }
    }
}

pub type Computations = HashMap<OpId, (StateLabel, OpOut)>;

/// The PocketIcApiState has a vector with elements of InstanceState.
/// When an operation is bound to an instance, the corresponding element in the
/// vector is replaced by a Busy variant which contains information about the
/// computation that is currently running. Afterwards, the instance is put back as
/// Available.
pub enum InstanceState {
    Busy {
        state_label: StateLabel,
        op_id: OpId,
    },
    Available(PocketIc),
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateError {
    message: String,
}

pub type UpdateResult = std::result::Result<UpdateReply, UpdateError>;

/// An operation bound to an instance can be dispatched, which updates the instance.
/// If the instance is already busy with an operation, the initial state and that operation
/// are returned.
/// If the result can be read from a cache, or if the computation is a fast read, an Output is
/// returned directly.
/// If the computation can be run and takes longer, a Busy variant is returned, containing the
/// requested op and the initial state.
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateReply {
    /// The requested instance is busy executing another update.
    Busy {
        state_label: StateLabel,
        op_id: OpId,
    },
    /// The requested instance is busy executing this current update.
    Started {
        state_label: StateLabel,
        op_id: OpId,
    },
    // This request is either cached or quickly executable, so we return
    // the output immediately.
    Output(OpOut),
}

impl UpdateReply {
    pub fn get_in_progress(&self) -> Option<(StateLabel, OpId)> {
        match self {
            Self::Busy { state_label, op_id } => Some((state_label.clone(), op_id.clone())),
            Self::Started { state_label, op_id } => Some((state_label.clone(), op_id.clone())),
            _ => None,
        }
    }
}

/// This trait lets us put a mock of the pocket_ic into the PocketIcApiState.
pub trait HasStateLabel {
    fn get_state_label(&self) -> StateLabel;
}

impl ApiState {
    /// For polling:
    /// The client lib dispatches a long running operation and gets a Busy {state_label, op_id}.
    /// It then polls on that via this state tree api function.
    pub fn read_result(
        graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
        state_label: &StateLabel,
        op_id: &OpId,
    ) -> Option<(StateLabel, OpOut)> {
        if let Some((new_state_label, op_out)) = graph.try_read().ok()?.get(state_label)?.get(op_id)
        {
            Some((new_state_label.clone(), op_out.clone()))
        } else {
            None
        }
    }

    pub async fn add_instance(&self, instance: PocketIc) -> InstanceId {
        let mut instances = self.instances.write().await;
        instances.push(Mutex::new(InstanceState::Available(instance)));
        let mut progress_threads = self.progress_threads.write().await;
        progress_threads.push(Mutex::new(None));
        instances.len() - 1
    }

    pub async fn delete_instance(&self, instance_id: InstanceId) {
        let instances = self.instances.read().await;
        let mut instance_state = instances[instance_id].lock().await;
        if let InstanceState::Available(pocket_ic) =
            std::mem::replace(&mut *instance_state, InstanceState::Deleted)
        {
            self.drop_sender.send(pocket_ic).unwrap();
        }
        let progress_threads = self.progress_threads.read().await;
        let mut progress_thread = progress_threads[instance_id].lock().await;
        if let Some(t) = progress_thread.take() {
            t.sender.send(()).await.unwrap();
            t.handle.await.unwrap();
        }
    }

    pub async fn auto_progress(&self, instance_id: InstanceId) {
        let progress_threads = self.progress_threads.read().await;
        let mut progress_thread = progress_threads[instance_id].lock().await;
        let instances = self.instances.clone();
        let graph = self.graph.clone();
        let drop_sender = self.drop_sender.clone();
        let sync_wait_time = self.sync_wait_time;
        if progress_thread.is_none() {
            let (tx, mut rx) = mpsc::channel::<()>(1);
            let handle = spawn(async move {
                loop {
                    use std::time::Instant;
                    let start = Instant::now();
                    let cur_op = SetTimeAndTick(SystemTime::now());
                    match Self::update_instances_with_timeout(
                        instances.clone(),
                        drop_sender.clone(),
                        cur_op.into(),
                        instance_id,
                        sync_wait_time,
                    )
                    .await
                    {
                        Ok(UpdateReply::Busy { .. }) => {}
                        Ok(UpdateReply::Output(_)) => {}
                        Ok(UpdateReply::Started { state_label, op_id }) => loop {
                            if Self::read_result(graph.clone(), &state_label, &op_id).is_some() {
                                break;
                            }
                            sleep(Duration::from_millis(100)).await;
                        },
                        Err(_) => {}
                    }
                    let duration = start.elapsed();
                    sleep(std::cmp::max(duration, MIN_TICK_DELAY)).await;
                    match rx.try_recv() {
                        Ok(_) | Err(TryRecvError::Disconnected) => {
                            break;
                        }
                        Err(TryRecvError::Empty) => {}
                    }
                }
            });
            *progress_thread = Some(ProgressThread { handle, sender: tx });
        }
    }

    pub async fn stop_progress(&self, instance_id: InstanceId) {
        let progress_threads = self.progress_threads.read().await;
        let mut progress_thread = progress_threads[instance_id].lock().await;
        if let Some(t) = progress_thread.take() {
            t.sender.send(()).await.unwrap();
            t.handle.await.unwrap();
        }
    }

    pub async fn list_instance_states(&self) -> Vec<String> {
        let instances = self.instances.read().await;
        let mut res = vec![];

        for instance_state in &*instances {
            let instance_state = &*instance_state.lock().await;
            match instance_state {
                InstanceState::Busy { state_label, op_id } => {
                    res.push(format!("Busy({:?}, {:?})", state_label, op_id))
                }
                InstanceState::Available(_) => res.push("Available".to_string()),
                InstanceState::Deleted => res.push("Deleted".to_string()),
            }
        }
        res
    }

    /// An operation bound to an instance (a Computation) can update the PocketIC state.
    ///
    /// * If the instance is busy executing an operation, the call returns [UpdateReply::Busy]
    /// immediately. In that case, the state label and operation id contained in the result
    /// indicate that the instance is busy with a previous operation.
    ///
    /// * If the instance is available and the computation exceeds a (short) timeout,
    /// [UpdateReply::Busy] is returned.
    ///
    /// * If the computation finished within the timeout, [UpdateReply::Output] is returned
    /// containing the result.
    ///
    /// Operations are _not_ queued by default. Thus, if the instance is busy with an existing operation,
    /// the client has to retry until the operation is done. Some operations for which the client
    /// might be unable to retry are exceptions to this rule and they are queued up implicitly
    /// by a retry mechanism inside PocketIc.
    pub async fn update<O>(&self, op: Arc<O>, instance_id: InstanceId) -> UpdateResult
    where
        O: Operation + Send + Sync + 'static,
    {
        self.update_with_timeout(op, instance_id, None).await
    }

    /// Same as [Self::update] except that the timeout can be specified manually. This is useful in
    /// cases when clients want to enforce a long-running blocking call.
    pub async fn update_with_timeout<O>(
        &self,
        op: Arc<O>,
        instance_id: InstanceId,
        sync_wait_time: Option<Duration>,
    ) -> UpdateResult
    where
        O: Operation + Send + Sync + 'static,
    {
        let sync_wait_time = sync_wait_time.unwrap_or(self.sync_wait_time);
        Self::update_instances_with_timeout(
            self.instances.clone(),
            self.drop_sender.clone(),
            op,
            instance_id,
            sync_wait_time,
        )
        .await
    }

    /// Same as [Self::update] except that the timeout can be specified manually. This is useful in
    /// cases when clients want to enforce a long-running blocking call.
    async fn update_instances_with_timeout<O>(
        instances: Arc<RwLock<Vec<Mutex<InstanceState>>>>,
        drop_sender: mpsc::UnboundedSender<PocketIc>,
        op: Arc<O>,
        instance_id: InstanceId,
        sync_wait_time: Duration,
    ) -> UpdateResult
    where
        O: Operation + Send + Sync + 'static,
    {
        let op_id = op.id().0;
        trace!(
            "update_with_timeout::start instance_id={} op_id={}",
            instance_id,
            op_id,
        );
        let instances_cloned = instances.clone();
        let instances_locked = instances_cloned.read().await;
        let (bg_task, busy_outcome) = if let Some(instance_mutex) =
            instances_locked.get(instance_id)
        {
            let mut instance_state = instance_mutex.lock().await;
            // If this instance is busy, return the running op and initial state
            match &*instance_state {
                InstanceState::Deleted => {
                    return Err(UpdateError {
                        message: "Instance was deleted".to_string(),
                    });
                }
                // TODO: cache lookup possible with this state_label and our own op_id
                InstanceState::Busy { state_label, op_id } => {
                    return Ok(UpdateReply::Busy {
                        state_label: state_label.clone(),
                        op_id: op_id.clone(),
                    });
                }
                InstanceState::Available(pocket_ic) => {
                    // move pocket_ic out

                    let state_label = pocket_ic.get_state_label();
                    let op_id = op.id();
                    let busy = InstanceState::Busy {
                        state_label: state_label.clone(),
                        op_id: op_id.clone(),
                    };
                    let InstanceState::Available(mut pocket_ic) =
                        std::mem::replace(&mut *instance_state, busy)
                    else {
                        unreachable!()
                    };

                    let bg_task = {
                        let old_state_label = state_label.clone();
                        let op_id = op_id.clone();
                        let drop_sender = drop_sender.clone();
                        move || {
                            trace!(
                                "bg_task::start instance_id={} state_label={:?} op_id={}",
                                instance_id,
                                old_state_label,
                                op_id.0,
                            );
                            let result = op.compute(&mut pocket_ic);
                            let instances = instances.blocking_read();
                            let mut instance_state = instances[instance_id].blocking_lock();
                            if let InstanceState::Deleted = &*instance_state {
                                drop_sender.send(pocket_ic).unwrap();
                            } else {
                                *instance_state = InstanceState::Available(pocket_ic);
                            }
                            trace!("bg_task::end instance_id={} op_id={}", instance_id, op_id.0);
                            result
                        }
                    };

                    // cache miss: replace pocket_ic instance in the vector with Busy
                    (bg_task, UpdateReply::Started { state_label, op_id })
                }
            }
        } else {
            return Err(UpdateError {
                message: "Instance not found".to_string(),
            });
        };
        // drop lock, otherwise we end up with a deadlock
        std::mem::drop(instances_locked);

        // We schedule a blocking background task on the tokio runtime. Note that if all
        // blocking workers are busy, the task is put on a queue (which is what we want).
        //
        // Note: One issue here is that we drop the join handle "on the floor". Threads
        // that are not awaited upon before exiting the process are known to cause spurios
        // issues. This should not be a problem as the tokio Executor will wait
        // indefinitively for threads to return, unless a shutdown timeout is configured.
        //
        // See: https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
        let bg_handle = spawn_blocking(bg_task);

        // if the operation returns "in time", we return the result, otherwise we indicate to the
        // client that the instance is busy.
        //
        // note: this assumes that cancelling the JoinHandle does not stop the execution of the
        // background task. This only works because the background thread, in this case, is a
        // kernel thread.
        if let Ok(op_out) = time::timeout(sync_wait_time, bg_handle).await {
            trace!(
                "update_with_timeout::synchronous instance_id={} op_id={}",
                instance_id,
                op_id,
            );
            return Ok(UpdateReply::Output(op_out.expect("join failed!")));
        }

        trace!(
            "update_with_timeout::timeout instance_id={} op_id={}",
            instance_id,
            op_id,
        );
        Ok(busy_outcome)
    }
}

impl std::fmt::Debug for InstanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { state_label, op_id } => {
                write!(f, "Busy {{ {state_label:?}, {op_id:?} }}")?
            }
            Self::Available(pic) => write!(f, "Available({:?})", pic.get_state_label())?,
            Self::Deleted => write!(f, "Deleted")?,
        }
        Ok(())
    }
}

impl std::fmt::Debug for ApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let instances = self.instances.blocking_read();
        let graph = self.graph.blocking_read();

        writeln!(f, "Instances:")?;
        for (idx, instance) in instances.iter().enumerate() {
            writeln!(f, "  [{idx}] {instance:?}")?;
        }

        writeln!(f, "Graph:")?;
        for (k, v) in graph.iter() {
            writeln!(f, "  {k:?} => {v:?}")?;
        }
        Ok(())
    }
}
