use crate::{
    output::{self, BackgroundRing, Capture},
    process::{self, ProcessControl},
    protocol::{self, DoneStatus, HostFrame, Stream, WorkerFrame},
    *,
};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::{
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub type Execution = Pin<Box<dyn Future<Output = Result<Outcome, Error>> + Send>>;
type Cleanup = Pin<Box<dyn Future<Output = Result<ResetOutcome, Error>> + Send>>;
type CleanupState = Option<Result<(), String>>;

#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
    _owner: Arc<Owner>,
}
struct Owner(Weak<Inner>);
impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            inner.retire_all("Python runtime was dropped");
        }
    }
}
struct Inner {
    config: Config,
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    closed: bool,
    next_binding: u64,
    next_generation: u64,
    bindings: HashMap<String, Binding>,
    workers: HashMap<u64, Worker>,
}
struct Binding {
    id: u64,
    launch: LaunchSpec,
    lifetime: CancellationToken,
    detached: CancellationToken,
    closed: bool,
    generation: Option<u64>,
    state_loss_reason: Option<String>,
}
struct Worker {
    key: String,
    binding: u64,
    phase: WorkerPhase,
    next_execution: u64,
    sender: mpsc::Sender<Call>,
    retire: CancellationToken,
    _control: ProcessControl,
    cleanup: watch::Receiver<CleanupState>,
}
#[derive(Clone)]
pub struct TaskHandle {
    inner: Arc<Inner>,
    key: String,
    binding: u64,
}
struct Call {
    execution_id: u64,
    code: String,
    cancel: CancellationToken,
    result: oneshot::Sender<Result<Outcome, Error>>,
    capture: Capture,
    value: Option<String>,
    traceback: Option<String>,
    loss: Option<String>,
    identity: Option<RuntimeIdentity>,
    started: Instant,
}
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Runtime {
    pub fn new(config: Config) -> Self {
        let inner = Arc::new(Inner {
            config,
            state: Mutex::new(State::default()),
        });
        Self {
            _owner: Arc::new(Owner(Arc::downgrade(&inner))),
            inner,
        }
    }
    /// Installs an immutable binding without starting an interpreter.
    pub fn bind(
        &self,
        key: impl Into<String>,
        mut launch: LaunchSpec,
        lifetime: CancellationToken,
    ) -> Result<TaskHandle, Error> {
        let key = key.into();
        if key.is_empty() || key.len() > 1024 {
            return Err(Error::InvalidInput(
                "Python task key must contain 1..=1024 bytes".into(),
            ));
        }
        if lifetime.is_cancelled() {
            return Err(Error::Retired);
        }
        if !launch.cwd.is_absolute() {
            return Err(Error::InvalidInput(
                "Python task root must be absolute".into(),
            ));
        }
        launch.cwd = std::fs::canonicalize(&launch.cwd).map_err(|error| {
            Error::Unavailable(format!("Python task root is unavailable: {error}"))
        })?;
        if !launch.cwd.is_dir() {
            return Err(Error::InvalidInput(
                "Python task root is not a directory".into(),
            ));
        }
        let mut state = self.inner.state.lock().unwrap();
        if state.closed {
            return Err(Error::Retired);
        }
        let prior_loss = if let Some(binding) = state.bindings.get(&key) {
            if !binding.closed && !binding.lifetime.is_cancelled() {
                if binding.launch != launch {
                    return Err(Error::LaunchMismatch);
                }
                return Ok(TaskHandle {
                    inner: self.inner.clone(),
                    key,
                    binding: binding.id,
                });
            }
            if binding.generation.is_some() {
                return Err(Error::CleanupPending);
            }
            binding.state_loss_reason.clone()
        } else {
            None
        };
        state.next_binding += 1;
        let id = state.next_binding;
        let detached = CancellationToken::new();
        state.bindings.insert(
            key.clone(),
            Binding {
                id,
                launch,
                lifetime: lifetime.clone(),
                detached: detached.clone(),
                closed: false,
                generation: None,
                state_loss_reason: prior_loss,
            },
        );
        drop(state);
        let handle = TaskHandle {
            inner: self.inner.clone(),
            key: key.clone(),
            binding: id,
        };
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            tokio::select! {
                _ = detached.cancelled() => {},
                _ = lifetime.cancelled() => if let Some(inner) = weak.upgrade() { inner.retire_binding(&key, id, "Python owner context was retired", true); },
            }
        });
        Ok(handle)
    }
    pub fn snapshot(&self) -> CapacitySnapshot {
        self.inner.snapshot()
    }
    /// Fences admission synchronously; the returned future only observes cleanup.
    pub fn shutdown(
        &self,
        reason: impl Into<String>,
    ) -> impl Future<Output = Result<(), Error>> + Send + 'static {
        let receivers = self.inner.retire_all(&reason.into());
        let timeout = self.inner.config.cleanup_timeout;
        async move {
            let wait = async {
                for receiver in receivers {
                    observe_cleanup(receiver).await?;
                }
                Ok(())
            };
            tokio::time::timeout(timeout, wait)
                .await
                .map_err(|_| Error::CleanupPending)?
        }
    }
}
impl Default for Runtime {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl TaskHandle {
    pub fn status(&self) -> TaskStatus {
        let state = self.inner.state.lock().unwrap();
        let Some(binding) = state
            .bindings
            .get(&self.key)
            .filter(|binding| binding.id == self.binding)
        else {
            return TaskStatus {
                generation: None,
                phase: WorkerPhase::Empty,
                closed: true,
                state_loss_reason: None,
            };
        };
        TaskStatus {
            generation: binding.generation,
            phase: binding
                .generation
                .and_then(|generation| {
                    state
                        .workers
                        .get(&generation)
                        .map(|worker| worker.phase.clone())
                })
                .unwrap_or(WorkerPhase::Empty),
            closed: binding.closed || binding.lifetime.is_cancelled(),
            state_loss_reason: binding.state_loss_reason.clone(),
        }
    }
    /// Admission happens synchronously, even if the returned future is never polled.
    pub fn execute(&self, code: impl Into<String>, cancel: CancellationToken) -> Execution {
        self.execute_guarded(code, cancel, || Ok(()))
    }
    /// The host guard spans the admission check and synchronous process spawn only.
    /// No guard (including a borrowed, non-Send mutex guard) enters the future.
    pub fn execute_guarded<G>(
        &self,
        code: impl Into<String>,
        cancel: CancellationToken,
        guard: impl FnOnce() -> Result<G, Error>,
    ) -> Execution {
        let code = code.into();
        let admitted = (|| {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if code.trim().is_empty() {
                return Err(Error::InvalidInput("Python code must not be empty".into()));
            }
            if code.len() > MAX_SOURCE_BYTES {
                return Err(Error::InvalidInput("Python code exceeds 256 KiB".into()));
            }
            // Validate worst-case JSON expansion before acquiring authority or capacity.
            protocol::encode(&HostFrame::Execute {
                generation: u64::MAX,
                execution_id: u64::MAX,
                code: code.clone(),
            })
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
            let _guard = guard()?;
            self.admit(code, cancel)
        })();
        match admitted {
            Err(error) => Box::pin(async move { Err(error) }),
            Ok((receiver, cancellation)) => {
                // Captured now: dropping an unpolled future cancels admitted work too.
                let guard = CancelOnDrop(cancellation);
                Box::pin(async move {
                    let _guard = guard;
                    receiver.await.unwrap_or_else(|_| {
                        Err(Error::WorkerLost(
                            "Python execution supervisor stopped unexpectedly".into(),
                        ))
                    })
                })
            }
        }
    }
    fn admit(
        &self,
        code: String,
        cancel: CancellationToken,
    ) -> Result<(oneshot::Receiver<Result<Outcome, Error>>, CancellationToken), Error> {
        let mut state = self.inner.state.lock().unwrap();
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if state.closed {
            return Err(Error::Retired);
        }
        let binding = state
            .bindings
            .get(&self.key)
            .filter(|binding| binding.id == self.binding)
            .ok_or(Error::Retired)?;
        if binding.closed || binding.lifetime.is_cancelled() {
            return Err(Error::Retired);
        }
        let generation = binding.generation;
        let launch = binding.launch.clone();
        let loss = binding.state_loss_reason.clone();
        let cancellation = cancel.child_token();
        let (tx, rx) = oneshot::channel();
        let mut call = Call {
            execution_id: 0,
            code,
            cancel: cancellation.clone(),
            result: tx,
            capture: Capture::default(),
            value: None,
            traceback: None,
            loss,
            identity: None,
            started: Instant::now(),
        };
        if let Some(generation) = generation {
            let worker = state
                .workers
                .get_mut(&generation)
                .ok_or(Error::CleanupPending)?;
            match worker.phase {
                WorkerPhase::Idle => {}
                WorkerPhase::Starting | WorkerPhase::Executing => return Err(Error::Busy),
                _ => return Err(Error::CleanupPending),
            }
            worker.next_execution += 1;
            call.execution_id = worker.next_execution;
            worker
                .sender
                .try_send(call)
                .map_err(|_| Error::WorkerLost("Python command receiver is unavailable".into()))?;
            worker.phase = WorkerPhase::Executing;
        } else {
            if state.workers.len() >= MAX_WORKERS {
                return Err(Error::Capacity {
                    holders: holders(&state),
                });
            }
            state.next_generation += 1;
            let generation = state.next_generation;
            call.execution_id = 1;
            let mut command = tokio::process::Command::new(&launch.python.executable);
            command
                .args(["-I", "-B", "-u"])
                .arg(&launch.python.worker)
                .arg("--generation")
                .arg(generation.to_string())
                .current_dir(&launch.cwd)
                .env_clear()
                .envs(&launch.env);
            let spawned =
                process::spawn(command, self.inner.config.retirement_grace).map_err(|error| {
                    Error::Unavailable(format!("Could not start bundled Python: {error}"))
                })?;
            let control = spawned.control.clone();
            let (sender, receiver) = mpsc::channel(1);
            let retire = CancellationToken::new();
            let (cleanup_tx, cleanup_rx) = watch::channel(None);
            state.workers.insert(
                generation,
                Worker {
                    key: self.key.clone(),
                    binding: self.binding,
                    phase: WorkerPhase::Starting,
                    next_execution: 1,
                    sender,
                    retire: retire.clone(),
                    _control: control,
                    cleanup: cleanup_rx,
                },
            );
            state.bindings.get_mut(&self.key).unwrap().generation = Some(generation);
            tokio::spawn(
                WorkerStart {
                    inner: self.inner.clone(),
                    generation,
                    launch,
                    spawned,
                    receiver,
                    retire,
                    cleanup_tx,
                    call,
                }
                .run(),
            );
        }
        state.bindings.get_mut(&self.key).unwrap().state_loss_reason = None;
        Ok((rx, cancellation))
    }
    /// Ends the current generation and keeps this logical task binding usable.
    /// Fencing happens before this method returns; no replacement is spawned.
    pub fn reset(&self, reason: impl Into<String>) -> Cleanup {
        self.cleanup(reason.into(), false)
    }
    /// Closes this exact binding forever, synchronously, including prepared handles.
    pub fn retire(&self, reason: impl Into<String>) -> Cleanup {
        self.cleanup(reason.into(), true)
    }
    fn cleanup(&self, reason: String, close: bool) -> Cleanup {
        let observation = self
            .inner
            .retire_binding(&self.key, self.binding, &reason, close);
        let timeout = self.inner.config.cleanup_timeout;
        Box::pin(async move {
            let (generation, receiver) = observation;
            if let Some(receiver) = receiver {
                tokio::time::timeout(timeout, observe_cleanup(receiver))
                    .await
                    .map_err(|_| Error::CleanupPending)??;
            }
            Ok(ResetOutcome {
                retired_generation: generation,
            })
        })
    }
}
fn holders(state: &State) -> Vec<Holder> {
    let mut holders: Vec<_> = state
        .workers
        .iter()
        .map(|(&generation, worker)| Holder {
            key: worker.key.clone(),
            generation,
            phase: worker.phase.clone(),
        })
        .collect();
    holders.sort_by_key(|holder| holder.generation);
    holders
}
impl Inner {
    fn snapshot(&self) -> CapacitySnapshot {
        CapacitySnapshot {
            limit: MAX_WORKERS,
            holders: holders(&self.state.lock().unwrap()),
        }
    }
    fn retire_binding(
        &self,
        key: &str,
        id: u64,
        reason: &str,
        close: bool,
    ) -> (Option<u64>, Option<watch::Receiver<CleanupState>>) {
        let mut state = self.state.lock().unwrap();
        let Some(binding) = state
            .bindings
            .get_mut(key)
            .filter(|binding| binding.id == id)
        else {
            return (None, None);
        };
        if close {
            binding.closed = true;
            binding.detached.cancel();
        }
        let generation = binding.generation;
        if close && generation.is_none() {
            state.bindings.remove(key);
            return (None, None);
        }
        if generation.is_some() {
            binding.state_loss_reason = Some(output::prefix(reason, 1024).into());
        }
        let receiver = generation
            .and_then(|generation| state.workers.get_mut(&generation))
            .map(|worker| {
                if !matches!(worker.phase, WorkerPhase::CleanupPending) {
                    worker.phase = WorkerPhase::Retiring;
                }
                worker.retire.cancel();
                worker.cleanup.clone()
            });
        (generation, receiver)
    }
    fn retire_all(&self, reason: &str) -> Vec<watch::Receiver<CleanupState>> {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        for binding in state.bindings.values_mut() {
            binding.closed = true;
            binding.detached.cancel();
            if binding.generation.is_some() {
                binding.state_loss_reason = Some(output::prefix(reason, 1024).into());
            }
        }
        state
            .bindings
            .retain(|_, binding| binding.generation.is_some());
        state
            .workers
            .values_mut()
            .map(|worker| {
                worker.phase = WorkerPhase::Retiring;
                worker.retire.cancel();
                worker.cleanup.clone()
            })
            .collect()
    }
    fn fence_generation(&self, generation: u64, reason: &str) {
        let mut state = self.state.lock().unwrap();
        let Some(worker) = state.workers.get_mut(&generation) else {
            return;
        };
        worker.phase = WorkerPhase::Retiring;
        worker.retire.cancel();
        let key = worker.key.clone();
        let id = worker.binding;
        if let Some(binding) = state
            .bindings
            .get_mut(&key)
            .filter(|binding| binding.id == id && binding.state_loss_reason.is_none())
        {
            binding.state_loss_reason = Some(output::prefix(reason, 1024).into());
        }
    }
}
async fn observe_cleanup(mut receiver: watch::Receiver<CleanupState>) -> Result<(), Error> {
    loop {
        if let Some(result) = receiver.borrow_and_update().clone() {
            return result.map_err(|_| Error::CleanupPending);
        }
        receiver
            .changed()
            .await
            .map_err(|_| Error::CleanupPending)?;
    }
}

struct OutputEvent {
    execution_id: Option<u64>,
    stream: Stream,
    text: String,
}
// A single FIFO preserves terminal/output wire ordering. Output permits bound
// text separately, leaving channel slots reserved for control frames.
struct WorkerEvent {
    frame: Result<WorkerFrame, String>,
    drops: TransportDrops,
    _output_budget: Option<OwnedSemaphorePermit>,
}
#[derive(Default, Clone, Copy)]
struct TransportDrops {
    stdout: u64,
    stderr: u64,
}
impl TransportDrops {
    fn add(&mut self, stream: Stream, count: usize) {
        let counter = match stream {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        };
        *counter = counter.saturating_add(count as u64);
    }
}
async fn queue_frame(
    frame: Result<WorkerFrame, String>,
    events: &mpsc::Sender<WorkerEvent>,
    budget: &Arc<Semaphore>,
    drops: &mut TransportDrops,
) -> bool {
    if let Ok(WorkerFrame::Output { stream, text, .. }) = &frame {
        let stream = *stream;
        let bytes = text.len();
        let Ok(permit) = budget.clone().try_acquire_owned() else {
            drops.add(stream, bytes);
            return true;
        };
        let event = WorkerEvent {
            frame,
            drops: *drops,
            _output_budget: Some(permit),
        };
        match events.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                drops.add(stream, bytes);
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    } else {
        events
            .send(WorkerEvent {
                frame,
                drops: *drops,
                _output_budget: None,
            })
            .await
            .is_ok()
    }
}
async fn read_frames(
    mut stdout: tokio::process::ChildStdout,
    generation: u64,
    events: mpsc::Sender<WorkerEvent>,
    sent_execution: Arc<AtomicU64>,
) {
    let budget = Arc::new(Semaphore::new(OUTPUT_QUEUE_BYTES / MAX_OUTPUT_CHUNK_BYTES));
    let mut drops = TransportDrops::default();
    let mut handshake_seen = false;
    loop {
        let mut frame = protocol::read_frame(&mut stdout)
            .await
            .map_err(|error| output::prefix(&error.to_string(), FINAL_BYTES).to_owned())
            .and_then(|frame| {
                if frame.generation() == generation {
                    Ok(frame)
                } else {
                    Err("Python worker sent a stale generation".into())
                }
            });
        if let Ok(WorkerFrame::Output { execution_id, .. }) = &frame
            && (!handshake_seen
                || execution_id
                    .is_some_and(|id| id == 0 || id > sent_execution.load(Ordering::Acquire)))
        {
            frame = Err(
                "Python output preceded readiness or used an unknown execution identity".into(),
            );
        }
        if matches!(&frame, Ok(WorkerFrame::Ready { .. })) {
            handshake_seen = true;
        }
        let failed = frame.is_err();
        if !queue_frame(frame, &events, &budget, &mut drops).await || failed {
            return;
        }
    }
}

struct WorkerStart {
    inner: Arc<Inner>,
    generation: u64,
    launch: LaunchSpec,
    spawned: process::SpawnedProcess,
    receiver: mpsc::Receiver<Call>,
    retire: CancellationToken,
    cleanup_tx: watch::Sender<CleanupState>,
    call: Call,
}
struct Actor {
    current: Option<Call>,
    background: BackgroundRing,
    ready: bool,
    last_execution: u64,
    transport_drops: TransportDrops,
    reported_transport_drops: TransportDrops,
    worker_drops: (u64, u64),
}
impl Actor {
    fn output(&mut self, event: OutputEvent) -> Result<(), String> {
        if !self.ready {
            return Err("Python output arrived before the ready handshake".into());
        }
        if event
            .execution_id
            .is_some_and(|id| id == 0 || id > self.last_execution)
        {
            return Err("Python output has an unknown execution identity".into());
        }
        if let Some(call) = self
            .current
            .as_mut()
            .filter(|call| Some(call.execution_id) == event.execution_id)
        {
            call.capture.push(event.stream, &event.text);
        } else {
            self.background
                .push(event.execution_id, event.stream, event.text);
        }
        Ok(())
    }
    fn terminal(
        &mut self,
        generation: u64,
        status: OutcomeStatus,
        elapsed_ms: u64,
    ) -> Option<(Call, Outcome)> {
        let mut call = self.current.take()?;
        let stdout = self
            .transport_drops
            .stdout
            .saturating_sub(self.reported_transport_drops.stdout);
        let stderr = self
            .transport_drops
            .stderr
            .saturating_sub(self.reported_transport_drops.stderr);
        self.reported_transport_drops = self.transport_drops;
        let outcome = Outcome {
            generation,
            execution_id: call.execution_id,
            status,
            stdout: std::mem::take(&mut call.capture.stdout),
            stderr: std::mem::take(&mut call.capture.stderr),
            value: call.value.take(),
            traceback: call.traceback.take(),
            elapsed_ms,
            dropped_stdout_bytes: call.capture.dropped_stdout.saturating_add(stdout),
            dropped_stderr_bytes: call.capture.dropped_stderr.saturating_add(stderr),
            background: self.background.take(),
            state_loss_reason: call.loss.take(),
            runtime: call.identity.take(),
            cleanup_pending: false,
        };
        Some((call, outcome))
    }
}
impl WorkerStart {
    async fn run(self) {
        let Self {
            inner,
            generation,
            launch,
            spawned,
            mut receiver,
            retire,
            cleanup_tx,
            call,
        } = self;
        let process::SpawnedProcess {
            mut stdin,
            stdout,
            mut stderr,
            control,
        } = spawned;
        log::debug!(
            "Python worker generation {generation} started (pid {})",
            control.pid()
        );
        let (events_tx, mut events_rx) =
            mpsc::channel(OUTPUT_QUEUE_BYTES / MAX_OUTPUT_CHUNK_BYTES + 16);
        let sent_execution = Arc::new(AtomicU64::new(0));
        let reader = tokio::spawn(read_frames(
            stdout,
            generation,
            events_tx.clone(),
            sent_execution.clone(),
        ));
        let (writer_tx, mut writer_rx) = mpsc::channel::<HostFrame>(2);
        let writer = tokio::spawn(async move {
            while let Some(frame) = writer_rx.recv().await {
                if let HostFrame::Execute { execution_id, .. } = &frame {
                    sent_execution.store(*execution_id, Ordering::Release);
                }
                if let Err(error) = protocol::write_frame(&mut stdin, &frame).await {
                    let _ = events_tx
                        .send(WorkerEvent {
                            frame: Err(format!("Python protocol write failed: {error}")),
                            drops: TransportDrops::default(),
                            _output_budget: None,
                        })
                        .await;
                    break;
                }
            }
        });
        let bootstrap = Arc::new(Mutex::new(String::new()));
        let bootstrap_buffer = bootstrap.clone();
        let stderr_reader = tokio::spawn(async move {
            let mut bytes = [0; 8192];
            while let Ok(size) = stderr.read(&mut bytes).await {
                if size == 0 {
                    break;
                }
                let mut buffer = bootstrap_buffer.lock().unwrap();
                let available = FINAL_BYTES.saturating_sub(buffer.len());
                buffer.push_str(output::prefix(
                    &String::from_utf8_lossy(&bytes[..size]),
                    available,
                ));
            }
        });
        let mut actor = Actor {
            current: Some(call),
            background: BackgroundRing::default(),
            ready: false,
            last_execution: 1,
            transport_drops: TransportDrops::default(),
            reported_transport_drops: TransportDrops::default(),
            worker_drops: (0, 0),
        };
        let deadline = tokio::time::sleep(inner.config.startup_timeout);
        tokio::pin!(deadline);
        let mut loss_status = OutcomeStatus::WorkerLost;
        let failure: String = loop {
            let cancellation = actor
                .current
                .as_ref()
                .map(|call| call.cancel.clone())
                .unwrap_or_default();
            tokio::select! {
                _ = retire.cancelled() => { loss_status = OutcomeStatus::Cancelled; break "Python worker was retired; its state was lost".into(); }
                _ = cancellation.cancelled(), if actor.current.is_some() => { loss_status = OutcomeStatus::Cancelled; break "An unfinished Python cell was cancelled; its worker state was lost".into(); }
                _ = &mut deadline, if !actor.ready => break "Bundled Python did not complete its ready handshake before the startup deadline".into(),
                result = control.cleanup() => {
                    break match result {
                        Ok(exit) => exit.startup_error.unwrap_or_else(|| format!("Python worker exited unexpectedly (code {:?}); its state was lost", exit.code)),
                        Err(message) => format!("Python process cleanup failed: {}", output::prefix(&message, 2048)),
                    };
                }
                event = events_rx.recv() => {
                    let Some(event) = event else { break "Python protocol reader stopped".into(); };
                    actor.transport_drops.stdout = actor.transport_drops.stdout.max(event.drops.stdout);
                    actor.transport_drops.stderr = actor.transport_drops.stderr.max(event.drops.stderr);
                    let frame = match event.frame { Ok(frame) => frame, Err(error) => break error };
                    match frame {
                        WorkerFrame::Ready { protocol_version, implementation, version, executable, cwd, .. } => {
                            if actor.ready { break "Python sent a duplicate ready handshake".into(); }
                            let executable = std::fs::canonicalize(executable);
                            let cwd = std::fs::canonicalize(cwd);
                            if protocol_version != 1 || implementation != launch.python.implementation || version != launch.python.version || executable.as_ref().ok() != Some(&launch.python.executable) || cwd.as_ref().ok() != Some(&launch.cwd) {
                                break "Bundled Python handshake did not match its declared protocol, implementation, version, executable, and task root".into();
                            }
                            actor.ready = true;
                            if let Some(call) = actor.current.as_mut() {
                                call.identity = Some(RuntimeIdentity { implementation, version, executable: launch.python.executable.clone(), cwd: launch.cwd.clone(), distribution: launch.python.distribution.clone() });
                                if let Err(error) = enqueue_execution(&inner, generation, call, &writer_tx) {
                                    if matches!(error, Error::Cancelled | Error::Retired) { loss_status = OutcomeStatus::Cancelled; }
                                    break error.to_string();
                                }
                            }
                        }
                        WorkerFrame::Result { execution_id, text, .. } => {
                            let Some(call) = actor.current.as_mut().filter(|call| actor.ready && call.execution_id == execution_id) else { break "Python result has no matching active execution".into(); };
                            if call.value.is_some() || call.traceback.is_some() { break "Python sent duplicate final content".into(); }
                            call.value = Some(text);
                        }
                        WorkerFrame::Error { execution_id, traceback, .. } => {
                            let Some(call) = actor.current.as_mut().filter(|call| actor.ready && call.execution_id == execution_id) else { break "Python error has no matching active execution".into(); };
                            if call.value.is_some() || call.traceback.is_some() { break "Python sent duplicate final content".into(); }
                            call.traceback = Some(traceback);
                        }
                        WorkerFrame::Done { execution_id, status, elapsed_ms, dropped_stdout_bytes, dropped_stderr_bytes, .. } => {
                            let Some(call) = actor.current.as_ref().filter(|call| actor.ready && call.execution_id == execution_id) else { break "Python terminal has no matching active execution".into(); };
                            if matches!(status, DoneStatus::Ok) && call.traceback.is_some() || matches!(status, DoneStatus::Error) && call.traceback.is_none() { break "Python terminal status contradicts its final content".into(); }
                            if dropped_stdout_bytes < actor.worker_drops.0 || dropped_stderr_bytes < actor.worker_drops.1 { break "Python output drop counters moved backwards".into(); }
                            let worker_delta = (dropped_stdout_bytes - actor.worker_drops.0, dropped_stderr_bytes - actor.worker_drops.1);
                            actor.worker_drops = (dropped_stdout_bytes, dropped_stderr_bytes);
                            // This lock is the completion-versus-retirement linearization point.
                            // Remove the call (and its cancellation watcher) before making it idle.
                            let mut state = inner.state.lock().unwrap();
                            if retire.is_cancelled() || call.cancel.is_cancelled() || matches!(status, DoneStatus::Cancelled) {
                                loss_status = OutcomeStatus::Cancelled;
                                break "An unfinished Python cell was cancelled; its worker state was lost".into();
                            }
                            let outcome_status = match status { DoneStatus::Ok => OutcomeStatus::Ok, DoneStatus::Error => OutcomeStatus::Error, DoneStatus::Cancelled => unreachable!() };
                            let (call, mut outcome) = actor.terminal(generation, outcome_status, elapsed_ms).unwrap();
                            outcome.dropped_stdout_bytes = outcome.dropped_stdout_bytes.saturating_add(worker_delta.0);
                            outcome.dropped_stderr_bytes = outcome.dropped_stderr_bytes.saturating_add(worker_delta.1);
                            if let Some(worker) = state.workers.get_mut(&generation) { worker.phase = WorkerPhase::Idle; }
                            let _ = call.result.send(Ok(outcome));
                        }
                        WorkerFrame::Fatal { message, .. } => break format!("Python protocol failed: {message}"),
                        WorkerFrame::Output { execution_id, stream, text, .. } => {
                            if let Err(error) = actor.output(OutputEvent { execution_id, stream, text }) { break error; }
                        }
                    }
                }
                next = receiver.recv(), if actor.current.is_none() => {
                    let Some(mut call) = next else { break "Python task command channel closed".into(); };
                    actor.last_execution = call.execution_id;
                    if call.cancel.is_cancelled() {
                        actor.current = Some(call); loss_status = OutcomeStatus::Cancelled;
                        break "An unfinished Python cell was cancelled; its worker state was lost".into();
                    }
                    let dispatch = enqueue_execution(&inner, generation, &mut call, &writer_tx);
                    actor.current = Some(call);
                    if let Err(error) = dispatch {
                        if matches!(error, Error::Cancelled | Error::Retired) { loss_status = OutcomeStatus::Cancelled; }
                        break error.to_string();
                    }
                }
            }
        };
        inner.fence_generation(generation, &failure);
        // The supervisor starts its sole grace window independently of pipe writes.
        control.retire();
        let _ = writer_tx.try_send(HostFrame::Shutdown { generation });
        drop(writer_tx);
        if actor.current.is_none() {
            actor.current = receiver.try_recv().ok();
        }
        let elapsed = actor
            .current
            .as_ref()
            .map(|call| call.started.elapsed().as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        let pending =
            actor
                .terminal(generation, loss_status, elapsed)
                .map(|(call, mut outcome)| {
                    outcome.state_loss_reason = Some(output::prefix(&failure, 2048).into());
                    if outcome.traceback.is_none() && outcome.status == OutcomeStatus::WorkerLost {
                        let diagnostics = bootstrap.lock().unwrap();
                        outcome.traceback = Some(
                            output::prefix(&format!("{failure}\n{diagnostics}"), FINAL_BYTES)
                                .into(),
                        );
                    }
                    (call.result, outcome)
                });
        finish_cleanup(
            inner,
            generation,
            control,
            cleanup_tx,
            pending,
            [reader, writer, stderr_reader],
        )
        .await;
    }
}

// Recheck known cancellation immediately before dispatch, including the interval
// between process spawn and readiness. This is also the exact binding retirement
// fence: reset/retire use this same lock. Cancellation after dispatch may have
// partial effects and is handled by supervised retirement.
fn enqueue_execution(
    inner: &Inner,
    generation: u64,
    call: &mut Call,
    writer: &mpsc::Sender<HostFrame>,
) -> Result<(), Error> {
    let mut state = inner.state.lock().unwrap();
    if call.cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let worker = state.workers.get(&generation).ok_or(Error::Retired)?;
    let binding = state
        .bindings
        .get(&worker.key)
        .filter(|binding| binding.id == worker.binding)
        .ok_or(Error::Retired)?;
    if state.closed
        || worker.retire.is_cancelled()
        || binding.closed
        || binding.lifetime.is_cancelled()
    {
        return Err(Error::Retired);
    }
    writer
        .try_send(HostFrame::Execute {
            generation,
            execution_id: call.execution_id,
            code: std::mem::take(&mut call.code),
        })
        .map_err(|_| Error::WorkerLost("Python command writer is unavailable".into()))?;
    state.workers.get_mut(&generation).unwrap().phase = WorkerPhase::Executing;
    Ok(())
}

async fn finish_cleanup(
    inner: Arc<Inner>,
    generation: u64,
    control: ProcessControl,
    cleanup_tx: watch::Sender<CleanupState>,
    mut pending: Option<(oneshot::Sender<Result<Outcome, Error>>, Outcome)>,
    tasks: [JoinHandle<()>; 3],
) {
    let cleanup = control.cleanup();
    tokio::pin!(cleanup);
    let result = tokio::select! {
        result = &mut cleanup => result,
        _ = tokio::time::sleep(inner.config.cleanup_timeout) => {
            if let Some(worker) = inner.state.lock().unwrap().workers.get_mut(&generation) { worker.phase = WorkerPhase::CleanupPending; }
            if let Some((sender, mut outcome)) = pending.take() { outcome.cleanup_pending = true; let _ = sender.send(Ok(outcome)); }
            cleanup.await
        }
    };
    match &result {
        Ok(_) => {
            let mut state = inner.state.lock().unwrap();
            if let Some(worker) = state.workers.remove(&generation)
                && let Some(binding) = state.bindings.get_mut(&worker.key).filter(|binding| {
                    binding.id == worker.binding && binding.generation == Some(generation)
                })
            {
                if binding.closed {
                    state.bindings.remove(&worker.key);
                } else {
                    binding.generation = None;
                }
            }
            let _ = cleanup_tx.send(Some(Ok(())));
        }
        Err(error) => {
            if let Some(worker) = inner.state.lock().unwrap().workers.get_mut(&generation) {
                worker.phase = WorkerPhase::CleanupPending;
            }
            log::error!(
                "Python generation {generation} cleanup remains pending: {}",
                output::prefix(error, 2048)
            );
            let _ = cleanup_tx.send(Some(Err(output::prefix(error, 2048).into())));
        }
    }
    if let Some((sender, mut outcome)) = pending {
        outcome.cleanup_pending = result.is_err();
        let _ = sender.send(Ok(outcome));
    }
    for task in tasks {
        task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn launch(root: &std::path::Path) -> LaunchSpec {
        LaunchSpec {
            python: PackagedPython {
                manifest: root.join("runtime.json"),
                executable: root.join("missing-python"),
                worker: root.join("worker.py"),
                implementation: "cpython".into(),
                version: "3.13.15".into(),
                distribution: "test".into(),
            },
            cwd: root.to_path_buf(),
            env: Default::default(),
        }
    }
    #[tokio::test]
    async fn preadmission_cancel_and_denied_guard_have_no_runtime_side_effects() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::default();
        let task = runtime
            .bind("task", launch(root.path()), CancellationToken::new())
            .unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let called = std::cell::Cell::new(false);
        assert!(matches!(
            task.execute_guarded("42", cancelled, || {
                called.set(true);
                Ok(())
            })
            .await,
            Err(Error::Cancelled)
        ));
        assert!(!called.get());
        assert!(matches!(
            task.execute_guarded("42", CancellationToken::new(), || Err::<(), _>(
                Error::Retired
            ))
            .await,
            Err(Error::Retired)
        ));
        assert!(runtime.snapshot().holders.is_empty());
        assert_eq!(task.status().generation, None);
    }
    #[tokio::test]
    async fn borrowed_non_send_host_guard_is_dropped_before_future_is_returned() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::default();
        let task = runtime
            .bind("task", launch(root.path()), CancellationToken::new())
            .unwrap();
        let fence = Mutex::new(());
        let execution =
            task.execute_guarded("42", CancellationToken::new(), || Ok(fence.lock().unwrap()));
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&execution);
        assert!(fence.try_lock().is_ok());
        assert!(matches!(execution.await, Err(Error::Unavailable(_))));
        assert!(runtime.snapshot().holders.is_empty());
    }
    #[tokio::test]
    async fn exact_binding_retirement_is_synchronous_and_cannot_be_undone_by_old_handles() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::default();
        let task = runtime
            .bind("task", launch(root.path()), CancellationToken::new())
            .unwrap();
        let cleanup = task.retire("archived");
        assert!(task.status().closed);
        assert!(matches!(
            task.execute("42", CancellationToken::new()).await,
            Err(Error::Retired)
        ));
        let replacement = runtime
            .bind("task", launch(root.path()), CancellationToken::new())
            .unwrap();
        cleanup.await.unwrap();
        drop(task.retire("old delayed capability"));
        assert!(!replacement.status().closed);
        assert_eq!(
            replacement
                .reset("already empty")
                .await
                .unwrap()
                .retired_generation,
            None
        );
    }
    #[tokio::test]
    async fn launch_mismatch_and_cancelled_lifetime_reject_before_spawn() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let runtime = Runtime::default();
        let lifetime = CancellationToken::new();
        let task = runtime
            .bind("task", launch(first.path()), lifetime.clone())
            .unwrap();
        assert!(matches!(
            runtime.bind("task", launch(second.path()), CancellationToken::new()),
            Err(Error::LaunchMismatch)
        ));
        lifetime.cancel();
        assert!(matches!(
            task.execute("42", CancellationToken::new()).await,
            Err(Error::Retired)
        ));
        assert!(runtime.snapshot().holders.is_empty());
    }
    #[tokio::test]
    async fn dropping_runtime_owner_fences_outliving_task_handles() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::default();
        let task = runtime
            .bind("task", launch(root.path()), CancellationToken::new())
            .unwrap();
        drop(runtime);
        assert!(matches!(
            task.execute("42", CancellationToken::new()).await,
            Err(Error::Retired)
        ));
    }
    #[test]
    fn attributed_late_and_raw_output_never_modify_foreground_capture() {
        let (tx, _rx) = oneshot::channel();
        let call = Call {
            execution_id: 2,
            code: String::new(),
            cancel: CancellationToken::new(),
            result: tx,
            capture: Capture::default(),
            value: None,
            traceback: None,
            loss: None,
            identity: None,
            started: Instant::now(),
        };
        let mut actor = Actor {
            current: Some(call),
            background: BackgroundRing::default(),
            ready: true,
            last_execution: 2,
            transport_drops: TransportDrops::default(),
            reported_transport_drops: TransportDrops::default(),
            worker_drops: (0, 0),
        };
        for (execution_id, text) in [(Some(1), "late"), (None, "raw"), (Some(2), "foreground")] {
            actor
                .output(OutputEvent {
                    execution_id,
                    stream: Stream::Stdout,
                    text: text.into(),
                })
                .unwrap();
        }
        let (_, outcome) = actor.terminal(1, OutcomeStatus::Ok, 0).unwrap();
        assert_eq!(outcome.stdout, "foreground");
        assert_eq!(outcome.background.chunks.len(), 2);
        assert!(actor.background.take().chunks.is_empty());
        assert!(
            actor
                .output(OutputEvent {
                    execution_id: Some(3),
                    stream: Stream::Stdout,
                    text: "future".into()
                })
                .is_err()
        );
    }
    #[tokio::test]
    async fn retired_empty_bindings_release_their_environments() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::default();
        for id in 0..100 {
            let task = runtime
                .bind(
                    format!("closed-{id}"),
                    launch(root.path()),
                    CancellationToken::new(),
                )
                .unwrap();
            task.retire("context ended").await.unwrap();
            assert!(matches!(
                task.execute("42", CancellationToken::new()).await,
                Err(Error::Retired)
            ));
        }
        assert!(runtime.inner.state.lock().unwrap().bindings.is_empty());
    }
    #[tokio::test]
    async fn fifo_keeps_late_output_after_the_completed_response() {
        let (sender, mut receiver) = mpsc::channel(32);
        let budget = Arc::new(Semaphore::new(16));
        let mut drops = TransportDrops::default();
        let (result, _receiver) = oneshot::channel();
        let call = Call {
            execution_id: 1,
            code: String::new(),
            cancel: CancellationToken::new(),
            result,
            capture: Capture::default(),
            value: None,
            traceback: None,
            loss: None,
            identity: None,
            started: Instant::now(),
        };
        let mut actor = Actor {
            current: Some(call),
            background: BackgroundRing::default(),
            ready: true,
            last_execution: 1,
            transport_drops: TransportDrops::default(),
            reported_transport_drops: TransportDrops::default(),
            worker_drops: (0, 0),
        };
        let output = |text: &str| {
            Ok(WorkerFrame::Output {
                generation: 1,
                execution_id: Some(1),
                stream: Stream::Stdout,
                text: text.into(),
            })
        };
        assert!(queue_frame(output("before"), &sender, &budget, &mut drops).await);
        assert!(
            queue_frame(
                Ok(WorkerFrame::Done {
                    generation: 1,
                    execution_id: 1,
                    status: DoneStatus::Ok,
                    elapsed_ms: 1,
                    dropped_stdout_bytes: 0,
                    dropped_stderr_bytes: 0
                }),
                &sender,
                &budget,
                &mut drops
            )
            .await
        );
        assert!(queue_frame(output("late"), &sender, &budget, &mut drops).await);
        let mut completed = None;
        for _ in 0..3 {
            let event = receiver.recv().await.unwrap();
            match event.frame.unwrap() {
                WorkerFrame::Output {
                    execution_id,
                    stream,
                    text,
                    ..
                } => actor
                    .output(OutputEvent {
                        execution_id,
                        stream,
                        text,
                    })
                    .unwrap(),
                WorkerFrame::Done { .. } => {
                    completed = actor
                        .terminal(1, OutcomeStatus::Ok, 1)
                        .map(|(_, outcome)| outcome)
                }
                _ => panic!("unexpected frame"),
            }
        }
        let completed = completed.unwrap();
        assert_eq!(completed.stdout, "before");
        assert!(completed.background.chunks.is_empty());
        assert_eq!(actor.background.take().chunks[0].text, "late");
    }
    #[tokio::test]
    async fn saturated_output_budget_preserves_control_and_terminal_drop_boundary() {
        let (sender, mut receiver) = mpsc::channel(32);
        let budget = Arc::new(Semaphore::new(1));
        let mut drops = TransportDrops::default();
        let output = || {
            Ok(WorkerFrame::Output {
                generation: 1,
                execution_id: None,
                stream: Stream::Stderr,
                text: "x".into(),
            })
        };
        assert!(queue_frame(output(), &sender, &budget, &mut drops).await);
        assert!(
            queue_frame(
                Ok(WorkerFrame::Done {
                    generation: 1,
                    execution_id: 1,
                    status: DoneStatus::Ok,
                    elapsed_ms: 0,
                    dropped_stdout_bytes: 0,
                    dropped_stderr_bytes: 0
                }),
                &sender,
                &budget,
                &mut drops
            )
            .await
        );
        assert!(queue_frame(output(), &sender, &budget, &mut drops).await);
        assert_eq!(drops.stderr, 1);
        let retained_output = receiver.recv().await.unwrap();
        let terminal = receiver.recv().await.unwrap();
        assert!(matches!(terminal.frame, Ok(WorkerFrame::Done { .. })));
        assert_eq!(
            terminal.drops.stderr, 0,
            "post-terminal drops belong to a later observation"
        );
        drop(retained_output);
        assert_eq!(budget.available_permits(), 1);
    }
}
