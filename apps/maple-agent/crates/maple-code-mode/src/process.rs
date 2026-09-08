//! Concrete ownership of one Python worker and its ordinary descendants.
//!
//! Caller deadlines observe a watch channel. They never cancel the supervisor's
//! platform cleanup wait or turn a reaped leader into a completed cleanup.

use std::{future::pending, io, process::Stdio, time::Duration};

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::{
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::watch,
    time::Instant,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessExit {
    pub code: Option<i32>,
    /// The worker needed termination rather than exiting during the grace period.
    pub forced: bool,
    /// Windows assignment/resume failed, but subsequent cleanup is confirmed.
    pub startup_error: Option<String>,
}

pub(crate) struct SpawnedProcess {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
    pub control: ProcessControl,
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessControl {
    pid: u32,
    grace: Duration,
    retirement: watch::Sender<Retirement>,
    cleanup: watch::Receiver<Cleanup>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Retirement {
    deadline: Option<Instant>,
}

#[derive(Clone, Debug)]
enum Cleanup {
    Pending,
    Complete(ProcessExit),
    /// Ownership and capacity must be retained after this observation.
    Failed(String),
}

impl ProcessControl {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Start the single grace window. Repeated requests never extend it.
    /// The worker layer sends the protocol shutdown request separately.
    pub(crate) fn retire(&self) {
        self.retirement.send_if_modified(|state| {
            if state.deadline.is_none() {
                state.deadline = Some(Instant::now() + self.grace);
                true
            } else {
                false
            }
        });
    }

    /// Only complete platform cleanup is an exit snapshot.
    #[cfg(test)]
    pub(crate) fn exited(&self) -> Option<ProcessExit> {
        match &*self.cleanup.borrow() {
            Cleanup::Complete(exit) => Some(exit.clone()),
            Cleanup::Pending | Cleanup::Failed(_) => None,
        }
    }

    /// An error means cleanup remains unconfirmed, not that capacity is free.
    /// A failed startup with confirmed cleanup is `Ok` with `startup_error` set.
    pub(crate) async fn cleanup(&self) -> Result<ProcessExit, String> {
        let mut cleanup = self.cleanup.clone();
        loop {
            match cleanup.borrow_and_update().clone() {
                Cleanup::Complete(exit) => return Ok(exit),
                Cleanup::Failed(error) => return Err(error),
                Cleanup::Pending => {}
            }
            cleanup.changed().await.map_err(|_| {
                "Python process supervisor stopped before confirming cleanup".to_owned()
            })?;
        }
    }
}

pub(crate) fn spawn(mut command: Command, grace: Duration) -> io::Result<SpawnedProcess> {
    // Check this before spawning: installing supervision must never panic because
    // the caller invoked the synchronous launch fence outside a Tokio runtime.
    let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut command = CommandWrap::from(command);
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());

    #[cfg(windows)]
    let job = {
        use windows::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        // Suspended creation makes assignment precede all worker execution.
        command.wrap(process_wrap::tokio::CreationFlags(
            CREATE_NO_WINDOW | CREATE_SUSPENDED,
        ));
        windows_job::Job::new()?
    };

    let child = command.spawn()?;
    // These are invariants of a freshly spawned, unpolled Tokio child whose
    // three streams were configured above. No fallible setup after spawn may
    // return an unowned child to the caller.
    let pid = child.id().expect("fresh Python worker has a process ID");
    let mut owner = OwnedProcess {
        child,
        #[cfg(unix)]
        group: GroupGuard { pid, armed: true },
        #[cfg(windows)]
        job,
    };
    let stdin = owner.child.stdin().take().expect("Python stdin is piped");
    let stdout = owner.child.stdout().take().expect("Python stdout is piped");
    let stderr = owner.child.stderr().take().expect("Python stderr is piped");

    #[cfg(unix)]
    let startup_error = None;
    #[cfg(windows)]
    let startup_error = owner
        .job
        .assign_and_resume(owner.child.inner_child(), pid)
        .err()
        .map(|error| format!("Python Job assignment or resume failed: {error}"));

    let control = install(owner, pid, grace, startup_error, &runtime);
    Ok(SpawnedProcess {
        stdin,
        stdout,
        stderr,
        control,
    })
}

fn install(
    owner: OwnedProcess,
    pid: u32,
    grace: Duration,
    startup_error: Option<String>,
    runtime: &tokio::runtime::Handle,
) -> ProcessControl {
    let (retirement, requests) = watch::channel(Retirement::default());
    let (completion, cleanup) = watch::channel(Cleanup::Pending);
    // Dropping this JoinHandle detaches, never aborts. The future already owns
    // the armed group/job and child before the launch fence can be released.
    runtime.spawn(supervise(owner, requests, completion, grace, startup_error));
    ProcessControl {
        pid,
        grace,
        retirement,
        cleanup,
    }
}

struct OwnedProcess {
    // Drop the group/job protection before releasing the direct-child handle.
    #[cfg(unix)]
    group: GroupGuard,
    #[cfg(windows)]
    job: windows_job::Job,
    child: Box<dyn ChildWrapper>,
}

impl OwnedProcess {
    fn raw_child(&mut self) -> &mut Child {
        // SAFETY: The only production wrappers are KillOnDrop plus Unix
        // ProcessGroup (Windows has no child wrapper). Tokio's cancellation-safe
        // leader wait leaves its cached status available to the ONE later group
        // wait. We do not mutate wrapper state, unwrap it, or call try_wait().
        unsafe { self.child.inner_child_mut() }
    }

    fn terminate(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        let descendants = self.group.terminate();
        #[cfg(windows)]
        let descendants = self.job.terminate();
        // Also reaches the direct worker if user code deliberately left the
        // Unix group, or Windows assignment failed while it was suspended.
        let leader = self.raw_child().start_kill();
        descendants.and(leader)
    }

    async fn wait_cleanup(&mut self) -> io::Result<std::process::ExitStatus> {
        // Never race, timeout, drop and restart this future. process-wrap 9.1.0
        // caches leader status before finishing ProcessGroupChild::wait().
        let status = self.child.wait().await?;
        #[cfg(windows)]
        self.job.wait_empty().await?;
        Ok(status)
    }

    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.group.armed = false;
        }
        // Windows closes an already-empty Job, keeping kill-on-close enabled.
    }
}

async fn supervise(
    mut owner: OwnedProcess,
    mut requests: watch::Receiver<Retirement>,
    completion: watch::Sender<Cleanup>,
    grace: Duration,
    startup_error: Option<String>,
) {
    let result = async {
        let observed = if startup_error.is_some() {
            Ok(true)
        } else {
            observe_leader(&mut owner, &mut requests, grace).await
        };
        // A successful or spontaneous leader exit still terminates remaining
        // ordinary descendants. Successful foreground execution is not a reason
        // to disarm this worker's process ownership.
        let termination = owner.terminate();
        let forced = observed.map_err(|error| format!("Python leader wait failed: {error}"))?;
        termination.map_err(|error| format!("Python group/Job termination failed: {error}"))?;
        let status = owner
            .wait_cleanup()
            .await
            .map_err(|error| format!("Python platform cleanup wait failed: {error}"))?;
        Ok::<_, String>(ProcessExit {
            code: status.code(),
            forced,
            startup_error,
        })
    }
    .await;

    match result {
        Ok(exit) => {
            owner.disarm();
            completion.send_replace(Cleanup::Complete(exit));
        }
        Err(error) => {
            log::error!(
                "Python cleanup remains pending (pid {:?}): {error}",
                owner.child.id()
            );
            completion.send_replace(Cleanup::Failed(error));
            // A failed group/Job wait cannot safely be restarted using cached
            // leader status. Keep the actual owner alive, along with its armed
            // drop protection. The service must also retain the capacity permit.
            // Runtime shutdown drops this future and invokes that protection;
            // it does not convert a cleanup failure to a confirmed exit.
            pending::<()>().await;
        }
    }
    drop(owner);
}

async fn observe_leader(
    owner: &mut OwnedProcess,
    requests: &mut watch::Receiver<Retirement>,
    grace: Duration,
) -> io::Result<bool> {
    let mut closed = false;
    let mut dropped_owner_deadline = None;
    loop {
        let request = *requests.borrow_and_update();
        let deadline = request.deadline.or(dropped_owner_deadline);
        tokio::select! {
            biased;
            // Only the raw Tokio wait is cancelled by a request. It is explicitly
            // cancellation-safe, unlike the later process-wrap group wait.
            exit = owner.raw_child().wait() => return exit.map(|_| false),
            change = requests.changed(), if !closed => {
                if change.is_err() {
                    closed = true;
                    dropped_owner_deadline = Some(Instant::now() + grace);
                }
            }
            _ = wait_deadline(deadline) => return Ok(true),
        }
    }
}

async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending().await,
    }
}

#[cfg(unix)]
struct GroupGuard {
    pid: u32,
    armed: bool,
}

#[cfg(unix)]
impl GroupGuard {
    fn terminate(&mut self) -> io::Result<()> {
        // SAFETY: This is the positive PID of our freshly created group leader;
        // negation selects that group, never the host's group or every process.
        let result = unsafe { libc::kill(-(self.pid as i32), libc::SIGKILL) };
        if result == 0 {
            // Do not retain a numeric PGID for another kill after its processes
            // have exited: the kernel can reuse it. Completion still requires
            // the separately retained wait, even if that wait later fails.
            self.armed = false;
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            self.armed = false;
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.terminate();
        }
    }
}

#[cfg(windows)]
mod windows_job {
    use std::{io, mem::size_of, time::Duration};

    use tokio::process::Child;
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
                QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    struct Handle(HANDLE);

    // SAFETY: This owned OS handle has no thread affinity. All accesses are
    // serialized by its owning supervisor, and it is closed exactly once.
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: The handle was returned by a successful Win32 creation call.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    pub(super) struct Job(Handle);

    impl Job {
        pub(super) fn new() -> io::Result<Self> {
            // SAFETY: No external pointers or inherited handles are supplied.
            let job = Self(Handle(unsafe { CreateJobObjectW(None, None) }?));
            let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: The pointer and byte count describe this initialized struct.
            unsafe {
                SetInformationJobObject(
                    job.0.0,
                    JobObjectExtendedLimitInformation,
                    &information as *const _ as _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            }?;
            Ok(job)
        }

        pub(super) fn assign_and_resume(&self, child: &Child, pid: u32) -> io::Result<()> {
            let process = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("suspended worker has no process handle"))?;
            // SAFETY: Both handles remain owned throughout assignment. The child
            // was created suspended, so it cannot create a descendant before this.
            unsafe { AssignProcessToJobObject(self.0.0, HANDLE(process)) }?;
            resume_initial_thread(pid)
        }

        pub(super) fn terminate(&self) -> io::Result<()> {
            // SAFETY: This is our owned Job, containing only this worker tree.
            unsafe { TerminateJobObject(self.0.0, 1) }.map_err(io::Error::other)
        }

        pub(super) async fn wait_empty(&mut self) -> io::Result<()> {
            // process-wrap 9.1.0's Job wait accepts any completion-port packet,
            // including NEW_PROCESS, as completion. Instead, after termination
            // and direct-child wait, observe this owned Job's actual active count.
            // Zero proves the Job is empty; errors preserve pending ownership.
            loop {
                let mut information = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
                // SAFETY: The output buffer is valid for the declared class/size.
                unsafe {
                    QueryInformationJobObject(
                        Some(self.0.0),
                        JobObjectBasicAccountingInformation,
                        &mut information as *mut _ as _,
                        size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                        None,
                    )
                }?;
                if information.ActiveProcesses == 0 {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    fn resume_initial_thread(pid: u32) -> io::Result<()> {
        // Tokio does not expose the primary thread handle. A suspended fresh
        // process cannot start additional threads before this assignment/resume.
        // Enumerate the snapshot before resuming to avoid touching later threads.
        let snapshot = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }?);
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        unsafe { Thread32First(snapshot.0, &mut entry) }?;
        let mut threads = Vec::new();
        loop {
            if entry.th32OwnerProcessID == pid {
                threads.push(Handle(unsafe {
                    OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)
                }?));
            }
            if let Err(error) = unsafe { Thread32Next(snapshot.0, &mut entry) } {
                if error.code() == ERROR_NO_MORE_FILES.to_hresult() {
                    break;
                }
                return Err(io::Error::other(error));
            }
        }
        if threads.is_empty() {
            return Err(io::Error::other("suspended worker has no thread to resume"));
        }
        for thread in threads {
            // SAFETY: These thread handles belong to the suspended worker only.
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        process::ExitStatus,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use tokio::{io::AsyncReadExt, sync::Notify, time::timeout};

    use super::*;

    #[derive(Debug)]
    struct DelayedWait {
        inner: Box<dyn ChildWrapper>,
        cached: Option<ExitStatus>,
        started: Arc<Notify>,
        release: Arc<Notify>,
        calls: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
        fail: bool,
    }

    impl Drop for DelayedWait {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl ChildWrapper for DelayedWait {
        fn inner(&self) -> &dyn ChildWrapper {
            self.inner.as_ref()
        }

        fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
            self.inner.as_mut()
        }

        fn into_inner(self: Box<Self>) -> Box<dyn ChildWrapper> {
            panic!("the supervisor must retain its wrapper")
        }

        fn wait(&mut self) -> Pin<Box<dyn Future<Output = io::Result<ExitStatus>> + Send + '_>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                // Deliberately reproduce process-wrap's early leader cache. A
                // cancelled/restarted wait would skip the remaining cleanup.
                if let Some(status) = self.cached {
                    return Ok(status);
                }
                let status = self.inner.wait().await?;
                self.cached = Some(status);
                self.started.notify_one();
                self.release.notified().await;
                if self.fail {
                    return Err(io::Error::other("injected remaining platform wait failure"));
                }
                Ok(status)
            })
        }
    }

    struct WaitProbe {
        control: ProcessControl,
        started: Arc<Notify>,
        release: Arc<Notify>,
        calls: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }

    fn delayed_wait(fail: bool) -> WaitProbe {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exit 0");
        let mut command = CommandWrap::from(command);
        command
            .wrap(KillOnDrop)
            .wrap(process_wrap::tokio::ProcessGroup::leader());
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let owner = OwnedProcess {
            child: Box::new(DelayedWait {
                inner: child,
                cached: None,
                started: started.clone(),
                release: release.clone(),
                calls: calls.clone(),
                dropped: dropped.clone(),
                fail,
            }),
            group: GroupGuard { pid, armed: true },
        };
        let control = install(
            owner,
            pid,
            Duration::from_millis(20),
            None,
            &tokio::runtime::Handle::current(),
        );
        WaitProbe {
            control,
            started,
            release,
            calls,
            dropped,
        }
    }

    #[tokio::test]
    async fn caller_timeouts_do_not_restart_the_remaining_platform_wait() {
        let probe = delayed_wait(false);
        timeout(Duration::from_secs(3), probe.started.notified())
            .await
            .unwrap();
        for _ in 0..2 {
            assert!(
                timeout(Duration::from_millis(20), probe.control.cleanup())
                    .await
                    .is_err()
            );
            assert!(probe.control.exited().is_none());
        }
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        assert!(!probe.dropped.load(Ordering::SeqCst));
        probe.release.notify_one();
        let exit = timeout(Duration::from_secs(3), probe.control.cleanup())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit.code, Some(0));
        assert!(!exit.forced);
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_remaining_wait_retains_owner_and_never_reports_cached_leader_exit() {
        let probe = delayed_wait(true);
        timeout(Duration::from_secs(3), probe.started.notified())
            .await
            .unwrap();
        probe.release.notify_one();
        let error = timeout(Duration::from_secs(3), probe.control.cleanup())
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("injected remaining platform wait failure"));
        assert!(probe.control.exited().is_none());
        probe.control.retire();
        tokio::task::yield_now().await;
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        assert!(!probe.dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn retirement_timeout_terminates_and_reaps_the_worker() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exec sleep 30");
        let process = spawn(command, Duration::from_millis(40)).unwrap();
        process.control.retire();
        let exit = timeout(Duration::from_secs(3), process.control.cleanup())
            .await
            .unwrap()
            .unwrap();
        assert!(exit.forced);
        assert_eq!(exit.code, None);
        assert!(exit.startup_error.is_none());
    }

    #[tokio::test]
    async fn spontaneous_leader_exit_terminates_ordinary_descendant_pipe_holders() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("sleep 30 & exit 0");
        let mut process = spawn(command, Duration::from_millis(40)).unwrap();
        let exit = timeout(Duration::from_secs(3), process.control.cleanup())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit.code, Some(0));
        assert!(!exit.forced);
        // EOF proves the ordinary descendant no longer holds this inherited
        // pipe. It does not assert we reaped a process that is not our child.
        let mut output = Vec::new();
        timeout(
            Duration::from_secs(3),
            process.stdout.read_to_end(&mut output),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn dropping_every_control_still_terminates_the_worker() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exec sleep 30");
        let mut process = spawn(command, Duration::from_millis(40)).unwrap();
        drop(process.control);
        let mut output = Vec::new();
        timeout(
            Duration::from_secs(3),
            process.stdout.read_to_end(&mut output),
        )
        .await
        .unwrap()
        .unwrap();
    }
}
