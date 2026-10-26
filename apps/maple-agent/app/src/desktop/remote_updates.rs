//! Bridge the host's acknowledged remote-update queue to this computer's
//! existing desktop installer. The client computer never enters this path.

use std::{future::Future, sync::Arc, time::Duration};

use gpui::Context;
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::Mutex;

use crate::{
    backend::AgentBackend,
    remote::update::{ReleaseIdentity, RemoteUpdateStatus},
    remote_updates::{
        AttachRequest, CompleteRequest, DesktopCommand, DesktopLease, DesktopRefreshCommand,
        PublishRequest, RefreshCompleteRequest, RefreshFailure, TakeRequest,
    },
    update::{Controller, Snapshot, State},
};

use super::{
    DesktopLifecycle, MapleApp, cancel_quit, persist_window_state, restart_installed_update,
};

#[derive(Default, Clone)]
pub(super) struct Bridge(Arc<Mutex<Option<Registration>>>);

struct Registration {
    account: String,
    lease: DesktopLease,
    published: Option<RemoteUpdateStatus>,
    completion: Option<CompleteRequest>,
}

pub(super) struct Pending {
    account: String,
    lease: String,
    command: DesktopCommand,
    bridge: Bridge,
}

struct PendingRefresh {
    account: String,
    lease: String,
    command: DesktopRefreshCommand,
    bridge: Bridge,
}

impl Bridge {
    /// Called by the existing five-second local status pump. No new timer or
    /// long-polling host request keeps an account/admission guard alive.
    pub(super) async fn poll(
        &self,
        backend: Arc<AgentBackend>,
        account: Option<String>,
        status: RemoteUpdateStatus,
        updater: Controller,
    ) -> Option<Pending> {
        let account = account.filter(|_| !backend.is_retired_after_update());
        let caller = backend.clone();
        let pending = self
            .poll_with(
                account.clone(),
                status,
                Duration::from_secs(5),
                move |method, params| {
                    let backend = caller.clone();
                    async move { backend.remote_setting(method, params).await }
                },
            )
            .await;
        if pending.is_none()
            && let Some(refresh) = self
                .take_refresh_with(account, {
                    let backend = backend.clone();
                    move |method, params| {
                        let backend = backend.clone();
                        async move { backend.remote_setting(method, params).await }
                    }
                })
                .await
        {
            let work_backend = backend.clone();
            let download_backend = backend.clone();
            drop(backend.spawn(async move {
                let expected = refresh.account.clone();
                let check_account = expected.clone();
                let check_backend = work_backend.clone();
                let check_updater = updater.clone();
                let completed = refresh
                    .run_with(
                        move |method, params| {
                            let backend = work_backend.clone();
                            async move { backend.remote_setting(method, params).await }
                        },
                        async move {
                            if check_backend.is_retired_after_update()
                                || check_backend.saved_user_id().as_deref()
                                    != Some(check_account.as_str())
                                || check_backend.store_state()
                                    != maple_agent::store::StoreState::Open
                            {
                                return Err(RefreshFailure::Unavailable);
                            }
                            check_updater.refresh_remote().await
                        },
                    )
                    .await;
                if completed
                    && download_backend.saved_user_id().as_deref() == Some(expected.as_str())
                    && !download_backend.is_retired_after_update()
                {
                    // Follow the execution desktop's saved download preference;
                    // this never installs or restarts either computer.
                    updater.download_if_automatic().await;
                }
            }));
        }
        pending
    }

    async fn take_refresh_with<C, F>(
        &self,
        account: Option<String>,
        mut call: C,
    ) -> Option<PendingRefresh>
    where
        C: FnMut(&'static str, serde_json::Value) -> F,
        F: Future<Output = Result<serde_json::Value, String>>,
    {
        let registration = self.0.lock().await;
        let current = registration
            .as_ref()
            .filter(|current| Some(&current.account) == account.as_ref())?;
        let command: Option<DesktopRefreshCommand> = tokio::time::timeout(
            Duration::from_secs(5),
            request(
                &mut call,
                "remote_update_refresh_take",
                TakeRequest {
                    expected_account: current.account.clone(),
                    lease: current.lease.id.clone(),
                },
            ),
        )
        .await
        .ok()?
        .ok()?;
        command.map(|command| PendingRefresh {
            account: current.account.clone(),
            lease: current.lease.id.clone(),
            command,
            bridge: self.clone(),
        })
    }

    async fn poll_with<C, F>(
        &self,
        account: Option<String>,
        status: RemoteUpdateStatus,
        completion_timeout: Duration,
        mut call: C,
    ) -> Option<Pending>
    where
        C: FnMut(&'static str, serde_json::Value) -> F,
        F: Future<Output = Result<serde_json::Value, String>>,
    {
        let mut registration = self.0.lock().await;
        let Some(account) = account else {
            *registration = None;
            return None;
        };
        if registration
            .as_ref()
            .is_some_and(|old| old.account != account)
        {
            *registration = None;
        }
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            if registration.is_none() {
                let lease = request(
                    &mut call,
                    "remote_update_attach",
                    AttachRequest {
                        expected_account: account.clone(),
                    },
                )
                .await?;
                *registration = Some(Registration {
                    account: account.clone(),
                    lease,
                    published: None,
                    completion: None,
                });
            }
            let current = registration.as_mut().expect("attached desktop lease");
            if current.published.as_ref() != Some(&status) || current.completion.is_some() {
                request::<(), _, _>(
                    &mut call,
                    "remote_update_publish",
                    PublishRequest {
                        expected_account: account.clone(),
                        lease: current.lease.id.clone(),
                        status: status.clone(),
                    },
                )
                .await?;
                current.published = Some(status);
            }
            // Do not claim another install until the previous completion was
            // acknowledged. The publication above renews the lease while that
            // report is pending; otherwise taking renews it with no command.
            let command: Option<DesktopCommand> = if current.completion.is_some() {
                None
            } else {
                request(
                    &mut call,
                    "remote_update_take",
                    TakeRequest {
                        expected_account: account.clone(),
                        lease: current.lease.id.clone(),
                    },
                )
                .await?
            };
            if let Some(completion) = &current.completion {
                // A lost completion acknowledgement must not strand Installing
                // while this lease keeps heartbeating. Retry only this report,
                // never the installation. A heartbeat/ownership failure above
                // drops the registration and its old-lease report instead.
                if matches!(
                    tokio::time::timeout(
                        completion_timeout,
                        request::<(), _, _>(&mut call, "remote_update_complete", completion),
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    current.completion = None;
                }
            }
            Ok::<_, String>(command.map(|command| Pending {
                account,
                lease: current.lease.id.clone(),
                command,
                bridge: self.clone(),
            }))
        })
        .await;
        match result {
            Ok(Ok(command)) => command,
            _ => {
                *registration = None;
                None
            }
        }
    }
}

impl PendingRefresh {
    async fn owned(&self) -> bool {
        self.bridge.0.lock().await.as_ref().is_some_and(|current| {
            current.account == self.account && current.lease.id == self.lease
        })
    }

    async fn run_with<C, F, Check>(self, mut call: C, check: Check) -> bool
    where
        C: FnMut(&'static str, serde_json::Value) -> F,
        F: Future<Output = Result<serde_json::Value, String>>,
        Check: Future<Output = Result<RemoteUpdateStatus, RefreshFailure>>,
    {
        if !self.owned().await {
            return false;
        }
        let result = check.await;
        if !self.owned().await {
            return false;
        }
        let checked = result.is_ok();
        matches!(
            tokio::time::timeout(
                Duration::from_secs(5),
                request::<(), _, _>(
                    &mut call,
                    "remote_update_refresh_complete",
                    RefreshCompleteRequest {
                        expected_account: self.account,
                        lease: self.lease,
                        id: self.command.id,
                        result,
                    },
                )
            )
            .await,
            Ok(Ok(()))
        ) && checked
    }
}

async fn request<R, C, F>(
    call: &mut C,
    method: &'static str,
    params: impl Serialize,
) -> Result<R, String>
where
    R: DeserializeOwned,
    C: FnMut(&'static str, serde_json::Value) -> F,
    F: Future<Output = Result<serde_json::Value, String>>,
{
    let params = serde_json::to_value(params).map_err(|_| "Invalid desktop updater request")?;
    let result = call(method, params).await?;
    serde_json::from_value(result).map_err(|_| "Invalid desktop updater response".into())
}

#[derive(Clone, Copy)]
struct Admission {
    store_ready: bool,
    store_open: bool,
    local_backend: bool,
    prompt_pending: bool,
    closing: bool,
}

fn may_begin(
    current_account: Option<&str>,
    expected_account: &str,
    snapshot: &Snapshot,
    target: &ReleaseIdentity,
    admission: Admission,
) -> bool {
    current_account == Some(expected_account)
        && admission.store_ready
        && admission.store_open
        && admission.local_backend
        && !admission.prompt_pending
        && !admission.closing
        && snapshot.state == State::Ready
        && snapshot.remote_candidate.as_ref() == Some(target)
}

async fn failed(backend: &AgentBackend, pending: Pending) {
    if backend.is_retired_after_update() {
        return;
    }
    queue_completion(pending).await;
}

async fn queue_completion(pending: Pending) {
    let mut registration = pending.bridge.0.lock().await;
    if let Some(current) = registration
        .as_mut()
        .filter(|current| current.account == pending.account && current.lease.id == pending.lease)
        && current.completion.is_none()
    {
        current.completion = Some(CompleteRequest {
            expected_account: pending.account,
            lease: pending.lease,
            operation_id: pending.command.operation_id,
            error: Some("The remote update did not complete. Open Maple on that computer to review it before trying again.".into()),
        });
    }
}

impl MapleApp {
    pub(super) fn begin_remote_install(&mut self, pending: Pending, cx: &mut Context<Self>) {
        let lifecycle = cx.try_global::<DesktopLifecycle>().cloned();
        let allowed = lifecycle.as_ref().is_some_and(|lifecycle| {
            may_begin(
                self.user_id.as_deref(),
                &pending.account,
                &self.updater.snapshot(),
                &pending.command.target,
                Admission {
                    store_ready: self.store_ready,
                    store_open: self.backend.store_state() == maple_agent::store::StoreState::Open,
                    local_backend: self.backend.remote_host().is_none(),
                    prompt_pending: self.restart_prompt_pending,
                    closing: lifecycle.closing.get(),
                },
            )
        });
        if !allowed {
            let backend = self.backend.clone();
            drop(
                self.backend
                    .spawn(async move { failed(&backend, pending).await }),
            );
            return;
        }
        // This is the execution computer's lifecycle. The controller's local
        // updater, selected remote chat and other paired computers are untouched.
        lifecycle
            .expect("checked desktop lifecycle")
            .closing
            .set(true);
        self.restart_prompt_pending = true;
        persist_window_state();
        let updater: Controller = self.updater.clone();
        let backend = self.backend.clone();
        let install_backend = backend.clone();
        let target = pending.command.target.clone();
        let operation = pending.command.operation_id.clone();
        let bridge = cx.spawn(async move |this, cx| {
            let controller = updater.clone();
            let installed = backend
                .spawn(async move {
                    controller
                        .install_remote(install_backend, target, operation)
                        .await
                })
                .await;
            // Once committed, do not issue any old-build host RPC, even if the
            // task failed after replacement. The retained controller path and
            // retirement fence are the restart authority.
            let committed = updater.restart_path();
            if committed.is_none() && !backend.is_retired_after_update() {
                let completing = backend.clone();
                // Reporting completion is independent of closing/reopening the
                // local UI. The status pump owns retry until acknowledgement or
                // lease loss; it cannot keep the desktop stuck in closing.
                drop(backend.spawn(async move { failed(&completing, pending).await }));
            }
            let _ = this.update(cx, |this, cx| {
                this.restart_prompt_pending = false;
                if let Some(path) = committed {
                    restart_installed_update(path, cx);
                } else {
                    // Errors and changed/withdrawn offers retain the old app.
                    // Never install the new offer without a fresh confirmation.
                    let _ = installed;
                    cancel_quit(cx);
                }
            });
        });
        crate::ui::task::retain(&self.remote_bridges, bridge);
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex as StdMutex};

    use super::*;
    use crate::remote::update::{OperationState, UpdateState};

    fn target() -> ReleaseIdentity {
        ReleaseIdentity {
            version: "0.0.7".into(),
            build: "551".into(),
            source_sha: "a".repeat(40),
            manifest_sha256: "b".repeat(64),
        }
    }

    fn ready_status() -> RemoteUpdateStatus {
        RemoteUpdateStatus {
            state: UpdateState::Ready,
            offered: Some(target()),
            blocked: None,
            operation: None,
        }
    }

    async fn refresh_call(
        broker: Arc<crate::remote_updates::Broker>,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match method {
            "remote_update_refresh_take" => serde_json::to_value(
                broker
                    .take_refresh(serde_json::from_value(params).unwrap())
                    .await?,
            )
            .map_err(|_| "fixture response".into()),
            "remote_update_refresh_complete" => {
                broker
                    .complete_refresh(serde_json::from_value(params).unwrap())
                    .await?;
                Ok(serde_json::Value::Null)
            }
            _ => panic!("unexpected refresh RPC {method}"),
        }
    }

    #[tokio::test]
    async fn queued_refresh_invokes_check_once_and_returns_its_fresh_result() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for outcome in [
            Ok(RemoteUpdateStatus {
                state: UpdateState::Current,
                ..Default::default()
            }),
            Ok(RemoteUpdateStatus {
                state: UpdateState::Available,
                offered: Some(target()),
                ..Default::default()
            }),
            Err(RefreshFailure::CheckFailed),
            Err(RefreshFailure::Busy),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let store = Arc::new(
                maple_agent::store::Store::open(maple_agent::store::StorePaths::new(
                    directory.path(),
                ))
                .unwrap(),
            );
            let broker = Arc::new(crate::remote_updates::Broker::new(Some(store)));
            let lease = broker.attach("alice".into()).await.unwrap();
            let bridge = Bridge::default();
            *bridge.0.lock().await = Some(Registration {
                account: "alice".into(),
                lease,
                published: Some(ready_status()),
                completion: None,
            });
            let waiting = tokio::spawn({
                let broker = broker.clone();
                async move {
                    broker
                        .request_refresh("alice".into(), "paired-peer".into())
                        .await
                }
            });
            let pending = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let caller = broker.clone();
                    if let Some(pending) = bridge
                        .take_refresh_with(Some("alice".into()), move |method, params| {
                            refresh_call(caller.clone(), method, params)
                        })
                        .await
                    {
                        break pending;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!waiting.is_finished());
            let calls = AtomicUsize::new(0);
            let expected = outcome.clone();
            let caller = broker.clone();
            let completed = pending
                .run_with(
                    move |method, params| refresh_call(caller.clone(), method, params),
                    async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        outcome
                    },
                )
                .await;
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(completed, expected.is_ok());
            let reply = waiting.await.unwrap();
            match expected {
                Ok(status) => assert_eq!(reply.unwrap(), status),
                Err(error) => assert_eq!(reply.unwrap_err(), error.message()),
            }
        }
    }

    #[tokio::test]
    async fn changed_desktop_owner_does_not_invoke_or_complete_an_old_refresh() {
        let bridge = Bridge::default();
        let pending = PendingRefresh {
            account: "old-account".into(),
            lease: "old-lease".into(),
            command: DesktopRefreshCommand {
                id: "old-check".into(),
            },
            bridge: bridge.clone(),
        };
        *bridge.0.lock().await = Some(Registration {
            account: "new-account".into(),
            lease: DesktopLease {
                id: "new-lease".into(),
            },
            published: None,
            completion: None,
        });
        assert!(
            !pending
                .run_with(
                    |_, _| async { panic!("stale completion must not be sent") },
                    async { panic!("stale check must not run") },
                )
                .await
        );
    }

    enum CompletionDelivery {
        LoseAcknowledgement,
        TimeoutBeforeWrite,
        Acknowledge,
    }

    struct HostFixture {
        account: String,
        lease: String,
        queued: Option<DesktopCommand>,
        operation: Option<(String, OperationState)>,
        deliveries: VecDeque<CompletionDelivery>,
        completions: Vec<serde_json::Value>,
        claims: usize,
        heartbeats: usize,
    }

    impl HostFixture {
        fn new(deliveries: impl IntoIterator<Item = CompletionDelivery>) -> Arc<StdMutex<Self>> {
            Arc::new(StdMutex::new(Self {
                account: "account-a".into(),
                lease: "lease-a".into(),
                queued: Some(DesktopCommand {
                    operation_id: "11111111-1111-4111-8111-111111111111".into(),
                    target: target(),
                }),
                operation: None,
                deliveries: deliveries.into_iter().collect(),
                completions: Vec::new(),
                claims: 0,
                heartbeats: 0,
            }))
        }

        async fn call(
            fixture: Arc<StdMutex<Self>>,
            method: &'static str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            {
                let mut host = fixture.lock().unwrap();
                if params["expected_account"] != host.account {
                    return Err("The signed-in account changed".into());
                }
                if method == "remote_update_attach" {
                    return Ok(serde_json::json!({ "id": host.lease }));
                }
                if params["lease"] != host.lease {
                    return Err("Desktop updater lease expired".into());
                }
                match method {
                    "remote_update_publish" => {
                        host.heartbeats += 1;
                        return Ok(serde_json::Value::Null);
                    }
                    "remote_update_take" => {
                        host.heartbeats += 1;
                        let command = host.queued.take();
                        if let Some(command) = &command {
                            host.claims += 1;
                            host.operation =
                                Some((command.operation_id.clone(), OperationState::Installing));
                        }
                        return serde_json::to_value(command).map_err(|error| error.to_string());
                    }
                    "remote_update_complete" => {
                        host.completions.push(params.clone());
                        match host
                            .deliveries
                            .pop_front()
                            .unwrap_or(CompletionDelivery::Acknowledge)
                        {
                            CompletionDelivery::TimeoutBeforeWrite => {}
                            delivery => {
                                let Some((operation, state)) = &mut host.operation else {
                                    return Err("Remote update request is unavailable".into());
                                };
                                if params["operation_id"] != *operation {
                                    return Err("Remote update ownership changed".into());
                                }
                                if *state == OperationState::Installing {
                                    *state = OperationState::Failed;
                                }
                                if matches!(delivery, CompletionDelivery::LoseAcknowledgement) {
                                    return Err("Completion acknowledgement disconnected".into());
                                }
                                return Ok(serde_json::Value::Null);
                            }
                        }
                    }
                    _ => panic!("unexpected updater method {method}"),
                }
            }
            std::future::pending().await
        }
    }

    async fn poll_fixture(
        bridge: &Bridge,
        fixture: &Arc<StdMutex<HostFixture>>,
        account: Option<&str>,
    ) -> Option<Pending> {
        let fixture = fixture.clone();
        bridge
            .poll_with(
                account.map(str::to_owned),
                ready_status(),
                Duration::from_millis(10),
                move |method, params| HostFixture::call(fixture.clone(), method, params),
            )
            .await
    }

    fn copy_pending(pending: &Pending) -> Pending {
        Pending {
            account: pending.account.clone(),
            lease: pending.lease.clone(),
            command: pending.command.clone(),
            bridge: pending.bridge.clone(),
        }
    }

    #[tokio::test]
    async fn lost_completion_ack_retries_only_the_same_report_before_claiming_more_work() {
        let bridge = Bridge::default();
        let fixture = HostFixture::new([CompletionDelivery::LoseAcknowledgement]);
        let pending = poll_fixture(&bridge, &fixture, Some("account-a"))
            .await
            .unwrap();
        let operation = pending.command.operation_id.clone();
        queue_completion(pending).await;

        assert!(
            poll_fixture(&bridge, &fixture, Some("account-a"))
                .await
                .is_none()
        );
        {
            let mut host = fixture.lock().unwrap();
            assert_eq!(
                host.operation,
                Some((operation.clone(), OperationState::Failed))
            );
            assert_eq!(host.completions.len(), 1);
            assert_eq!(host.claims, 1);
            host.queued = Some(DesktopCommand {
                operation_id: "22222222-2222-4222-8222-222222222222".into(),
                target: target(),
            });
        }
        assert!(
            poll_fixture(&bridge, &fixture, Some("account-a"))
                .await
                .is_none()
        );
        {
            let host = fixture.lock().unwrap();
            assert_eq!(host.completions.len(), 2);
            assert_eq!(host.completions[0], host.completions[1]);
            assert_eq!(host.completions[0]["operation_id"], operation);
            assert_eq!(host.claims, 1);
            assert!(host.heartbeats >= 4);
        }
        assert!(bridge.0.lock().await.as_ref().unwrap().completion.is_none());

        let next = poll_fixture(&bridge, &fixture, Some("account-a"))
            .await
            .unwrap();
        assert_eq!(
            next.command.operation_id,
            "22222222-2222-4222-8222-222222222222"
        );
        let host = fixture.lock().unwrap();
        assert_eq!(host.claims, 2);
        assert_eq!(host.completions.len(), 2);
    }

    #[tokio::test]
    async fn completion_timeout_keeps_the_report_and_heartbeat_without_reinstalling() {
        let bridge = Bridge::default();
        let fixture = HostFixture::new([CompletionDelivery::TimeoutBeforeWrite]);
        let pending = poll_fixture(&bridge, &fixture, Some("account-a"))
            .await
            .unwrap();
        let operation = pending.command.operation_id.clone();
        queue_completion(pending).await;
        assert!(
            poll_fixture(&bridge, &fixture, Some("account-a"))
                .await
                .is_none()
        );
        assert!(bridge.0.lock().await.as_ref().unwrap().completion.is_some());
        assert_eq!(
            fixture.lock().unwrap().operation,
            Some((operation.clone(), OperationState::Installing))
        );

        assert!(
            poll_fixture(&bridge, &fixture, Some("account-a"))
                .await
                .is_none()
        );
        let host = fixture.lock().unwrap();
        assert_eq!(host.operation, Some((operation, OperationState::Failed)));
        assert_eq!(host.completions.len(), 2);
        assert_eq!(host.completions[0], host.completions[1]);
        assert_eq!(host.claims, 1);
        assert!(host.heartbeats >= 4);
    }

    #[tokio::test]
    async fn completion_reports_cannot_cross_account_or_desktop_lease_changes() {
        let bridge = Bridge::default();
        let fixture = HostFixture::new([]);
        let pending = poll_fixture(&bridge, &fixture, Some("account-a"))
            .await
            .unwrap();
        let delayed = copy_pending(&pending);
        queue_completion(pending).await;
        {
            let mut host = fixture.lock().unwrap();
            host.account = "account-b".into();
            host.lease = "lease-b".into();
        }
        assert!(
            poll_fixture(&bridge, &fixture, Some("account-b"))
                .await
                .is_none()
        );
        queue_completion(delayed).await;
        let registration = bridge.0.lock().await;
        let current = registration.as_ref().unwrap();
        assert_eq!(current.account, "account-b");
        assert_eq!(current.lease.id, "lease-b");
        assert!(current.completion.is_none());
        assert!(fixture.lock().unwrap().completions.is_empty());
    }

    #[tokio::test]
    async fn lost_lease_or_retired_desktop_discards_old_completion_reports() {
        let bridge = Bridge::default();
        let fixture = HostFixture::new([]);
        let pending = poll_fixture(&bridge, &fixture, Some("account-a"))
            .await
            .unwrap();
        queue_completion(pending).await;
        fixture.lock().unwrap().lease = "lease-replacement".into();
        assert!(
            poll_fixture(&bridge, &fixture, Some("account-a"))
                .await
                .is_none()
        );
        assert!(bridge.0.lock().await.is_none());
        assert!(fixture.lock().unwrap().completions.is_empty());

        assert!(
            poll_fixture(&bridge, &fixture, Some("account-a"))
                .await
                .is_none()
        );
        let registration = bridge.0.lock().await;
        assert_eq!(registration.as_ref().unwrap().lease.id, "lease-replacement");
        drop(registration);
        assert!(poll_fixture(&bridge, &fixture, None).await.is_none());
        assert!(bridge.0.lock().await.is_none());
        assert!(fixture.lock().unwrap().completions.is_empty());
        assert_eq!(fixture.lock().unwrap().claims, 1);
    }

    #[test]
    fn acknowledged_command_cannot_cross_accounts_hosts_or_changed_candidates() {
        let target = target();
        let mut snapshot = Snapshot::initial();
        snapshot.state = State::Ready;
        snapshot.remote_candidate = Some(target.clone());
        let admission = Admission {
            store_ready: true,
            store_open: true,
            local_backend: true,
            prompt_pending: false,
            closing: false,
        };
        assert!(may_begin(
            Some("account-a"),
            "account-a",
            &snapshot,
            &target,
            admission
        ));
        assert!(!may_begin(
            Some("account-b"),
            "account-a",
            &snapshot,
            &target,
            admission
        ));
        assert!(!may_begin(
            Some("account-a"),
            "account-a",
            &snapshot,
            &target,
            Admission {
                local_backend: false,
                ..admission
            }
        ));
        assert!(!may_begin(
            Some("account-a"),
            "account-a",
            &snapshot,
            &target,
            Admission {
                store_open: false,
                ..admission
            }
        ));
        assert!(!may_begin(
            Some("account-a"),
            "account-a",
            &snapshot,
            &target,
            Admission {
                closing: true,
                ..admission
            }
        ));
        let mut changed = target.clone();
        changed.manifest_sha256 = "c".repeat(64);
        snapshot.remote_candidate = Some(changed);
        assert!(!may_begin(
            Some("account-a"),
            "account-a",
            &snapshot,
            &target,
            admission
        ));
    }
}
