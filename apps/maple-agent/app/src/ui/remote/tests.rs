use super::*;
use crate::remote::update::ReleaseIdentity;
use gpui::TestAppContext;

fn screen(cx: &mut TestAppContext) -> Entity<RemoteScreen> {
    cx.executor().allow_parking();
    let backend =
        Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
    // No status request or timer: drive state transitions without connecting
    // to a developer's host or changing pairing authority.
    cx.new(|cx| RemoteScreen {
        backend,
        account: "account-a".into(),
        active: false,
        status: Some(Status {
            name: "Test Mac".into(),
            enabled: true,
            ready: true,
            error: None,
            endpoint: Some("local".into()),
            peers: Default::default(),
        }),
        devices: None,
        selected_device: None,
        code_input: cx.new(|cx| TextInput::new("Pairing code", cx).pairing_code()),
        names: Default::default(),
        renaming: None,
        local_name_input: None,
        local_code: None,
        error: None,
        notice: None,
        busy: None,
        computer_updates: HashMap::new(),
        next_update_generation: 0,
        install_confirmation: None,
        bridges: RefCell::new(Vec::new()),
    })
}

fn device(key: &str) -> RemoteDevice {
    RemoteDevice {
        channel: "test".into(),
        endpoint: key.into(),
        name: key.into(),
        addresses: Default::default(),
        accepting_connections: true,
        window: Some(maple_sdk::remote::RemoteWindow {
            id: uuid::Uuid::new_v4(),
            expires_at: i64::MAX,
        }),
        expires_at: i64::MAX,
    }
}

#[gpui::test]
fn own_name_editor_is_visible_and_preserves_edits_during_status_refresh(cx: &mut TestAppContext) {
    struct Host(Entity<RemoteScreen>);
    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(950.)).h(px(850.)).child(self.0.clone())
        }
    }
    let screen = screen(cx);
    let (_, cx) = cx.add_window_view(|window, cx| {
        window.activate_window();
        crate::desktop::register_key_bindings(cx);
        Host(screen.clone())
    });
    cx.simulate_resize(gpui::size(px(950.), px(850.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("this-computer-name-editor").is_none());
    let rename = cx.debug_bounds("rename-this-computer").unwrap();
    cx.simulate_click(rename.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("this-computer-name-editor").is_some());
    screen.update(cx, |screen, cx| {
        let input = screen.local_name_input.as_ref().unwrap().clone();
        assert_eq!(input.read(cx).text(), "Test Mac");
        input.update(cx, |input, cx| input.set_text("Office Mac", cx));
        let mut status = screen.status.clone().unwrap();
        status.ready = false;
        screen.update_status(status, cx);
        assert_eq!(input.read(cx).text(), "Office Mac");
        input.update(cx, |input, cx| input.clear(cx));
        screen.save_this_computer_name(cx);
        assert!(screen.error.is_some());
        assert!(screen.busy.is_none(), "invalid input never sends a request");
    });
    cx.run_until_parked();
    let cancel = cx.debug_bounds("cancel-this-computer-name").unwrap();
    cx.simulate_click(cancel.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("this-computer-name-editor").is_none());
    screen.read_with(cx, |screen, _| {
        assert_eq!(screen.status.as_ref().unwrap().name, "Test Mac");
        assert!(
            screen.names.is_empty(),
            "own-name editing never sets a peer nickname"
        );
    });
}

#[gpui::test]
fn code_entry_requires_selection_and_does_not_follow_a_different_device(cx: &mut TestAppContext) {
    struct Host(Entity<RemoteScreen>);
    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(950.)).h(px(850.)).child(self.0.clone())
        }
    }
    let screen = screen(cx);
    let (_, cx) = cx.add_window_view(|window, cx| {
        window.activate_window();
        crate::desktop::register_key_bindings(cx);
        Host(screen.clone())
    });
    cx.simulate_resize(gpui::size(px(950.), px(850.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("pairing-code-entry").is_none());

    screen.update(cx, |screen, cx| {
        screen.devices = Some(vec![device("a"), device("b")]);
        cx.notify();
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("pairing-code-entry").is_none(),
        "discovery alone never opens code entry"
    );
    let choose = cx.debug_bounds("select-a").unwrap();
    cx.simulate_click(choose.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("pairing-code-entry").is_some());
    screen.update(cx, |screen, cx| {
        assert_eq!(screen.selected_device.as_deref(), Some("a"));
        screen
            .code_input
            .update(cx, |input, cx| input.set_text("ABCD-EFGH", cx));
    });
    cx.run_until_parked();
    let choose = cx.debug_bounds("select-b").unwrap();
    cx.simulate_click(choose.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    screen.update(cx, |screen, cx| {
        assert_eq!(screen.selected_device.as_deref(), Some("b"));
        assert!(screen.code_input.read(cx).text().is_empty());
        screen.pair_selected(cx);
        assert!(
            screen.busy.is_none(),
            "empty submission must not start a pairing attempt"
        );
        assert!(screen.error.is_some());
    });
    cx.run_until_parked();
    let cancel = cx.debug_bounds("cancel-code").unwrap();
    cx.simulate_click(cancel.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("pairing-code-entry").is_none());
    screen.update(cx, |screen, cx| {
        screen.pair_selected(cx);
        assert!(
            screen.busy.is_none(),
            "no selection must not start a pairing attempt"
        );
    });
}

#[gpui::test]
fn incoming_pairing_closes_the_code_but_an_outgoing_grant_does_not(cx: &mut TestAppContext) {
    let screen = screen(cx);
    screen.update(cx, |screen, cx| {
        screen.local_code = Some((
            "ABCD-EFGH".into(),
            Instant::now() + Duration::from_secs(300),
        ));
        let mut status = screen.status.clone().unwrap();
        status.peers.insert(
            "peer".into(),
            serde_json::from_value(serde_json::json!({
                "name": "Peer", "grant": vec![1; 16], "can_connect": true,
                "controls_this_computer": false, "pending_confirmation": null
            }))
            .unwrap(),
        );
        screen.update_status(status.clone(), cx);
        assert!(
            screen.local_code.is_some(),
            "outgoing access is not a new controller"
        );
        status.peers.get_mut("peer").unwrap().controls_this_computer = true;
        screen.update_status(status, cx);
        assert!(screen.local_code.is_none());
        assert!(screen.notice.as_ref().unwrap().contains("Computer paired"));
        assert!(screen.status.as_ref().unwrap().peers["peer"].controls_this_computer);
    });
}

#[gpui::test]
fn settings_reentry_keeps_the_open_window_with_existing_controllers(cx: &mut TestAppContext) {
    let screen = screen(cx);
    screen.update(cx, |screen, cx| {
        let mut status = screen.status.take().unwrap();
        status.peers.insert(
            "existing".into(),
            serde_json::from_value(serde_json::json!({
                "name": "Existing", "grant": vec![1; 16], "can_connect": false,
                "controls_this_computer": true, "pending_confirmation": null
            }))
            .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        screen.local_code = Some(("ABCD-EFGH".into(), deadline));
        screen.update_status(status.clone(), cx);
        assert_eq!(screen.local_code, Some(("ABCD-EFGH".into(), deadline)));
        assert!(screen.notice.is_none());
        // A real new grant after that baseline still closes the displayed code.
        status.peers.get_mut("existing").unwrap().grant = [2; 16];
        screen.update_status(status, cx);
        assert!(screen.local_code.is_none());
        assert!(screen.notice.as_ref().unwrap().contains("Computer paired"));
    });
}

#[gpui::test]
fn late_nickname_snapshot_updates_labels_without_replacing_the_active_editor(
    cx: &mut TestAppContext,
) {
    let screen = screen(cx);
    screen.update(cx, |screen, cx| {
        screen.rename("peer".into(), "Advertised hostname".into(), cx);
        let input = screen.renaming.as_ref().unwrap().1.clone();
        input.update(cx, |input, cx| input.set_text("Unsubmitted edit", cx));
        screen.set_names([("peer".into(), "Saved Office nickname".into())].into(), cx);
        assert_eq!(screen.names["peer"], "Saved Office nickname");
        assert_eq!(
            screen.renaming.as_ref().unwrap().1.entity_id(),
            input.entity_id()
        );
        assert_eq!(input.read(cx).text(), "Unsubmitted edit");
    });
}

fn target_release() -> ReleaseIdentity {
    ReleaseIdentity {
        version: "0.0.7".into(),
        build: "551".into(),
        source_sha: "1".repeat(40),
        manifest_sha256: "2".repeat(64),
    }
}

fn ready_peer() -> PeerInfo {
    PeerInfo {
        protocol: crate::remote::update::ProtocolInfo::current(),
        running: crate::remote::update::RunningApplication {
            version: "0.0.6".into(),
            build: Some("550".into()),
            source_sha: "0".repeat(40),
            platform: "linux-x86_64".into(),
        },
        update: crate::remote::update::RemoteUpdateStatus {
            state: UpdateState::Ready,
            offered: Some(target_release()),
            ..Default::default()
        },
    }
}

fn seed_update(screen: &mut RemoteScreen, key: &str, grant: [u8; 16]) {
    screen.status.as_mut().unwrap().peers.insert(
        key.into(),
        serde_json::from_value(serde_json::json!({
            "name": format!("Office {key}"), "grant": grant, "can_connect": true,
            "controls_this_computer": false, "pending_confirmation": null
        }))
        .unwrap(),
    );
    // Eligibility comes only from this remote's verified prepared candidate;
    // the controller does not hold a local updater snapshot or download.
    let mut update = ComputerUpdate {
        generation: 1,
        phase: PeerInfoPhase::Loaded,
        info: Some(ready_peer()),
        ..Default::default()
    };
    update.refresh_presentation();
    assert!(update.install_available);
    screen.computer_updates.insert(key.into(), update);
}

struct UpdateHost(Entity<RemoteScreen>);
impl Render for UpdateHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().w_full().h_full().p_4().child(
            self.0
                .update(cx, |screen, cx| screen.render_computer_update("a", cx)),
        )
    }
}

fn show_update(cx: &mut TestAppContext) -> (Entity<RemoteScreen>, &mut gpui::VisualTestContext) {
    let screen = screen(cx);
    screen.update(cx, |screen, _| seed_update(screen, "a", [1; 16]));
    let (_, cx) = cx.add_window_view(|window, cx| {
        window.activate_window();
        crate::desktop::register_key_bindings(cx);
        UpdateHost(screen.clone())
    });
    cx.simulate_resize(gpui::size(px(640.), px(600.)));
    cx.run_until_parked();
    (screen, cx)
}

#[test]
fn remote_eligibility_depends_on_the_remote_candidate_and_authority_only() {
    let mut info = ready_peer();
    // A release different from this client's own version remains eligible.
    // There is no controller-side candidate or updater state in this decision.
    info.update.offered.as_mut().unwrap().version = "99.0.0".into();
    info.update.offered.as_mut().unwrap().build = "999".into();
    info.update.offered.as_mut().unwrap().manifest_sha256 = "f".repeat(64);
    assert!(install_blocked(&info).is_none());

    info.update.state = UpdateState::Downloading;
    assert!(install_blocked(&info).is_some());
    info.update.state = UpdateState::Ready;
    info.update.offered = None;
    assert!(install_blocked(&info).is_some());

    let mut info = ready_peer();
    info.protocol.capabilities.clear();
    assert!(install_blocked(&info).is_some());

    let mut info = ready_peer();
    info.running.version = target_release().version;
    info.running.source_sha = target_release().source_sha;
    info.running.build = Some(target_release().build);
    assert!(
        install_blocked(&info).is_some(),
        "already-running target is not installed again"
    );
}

#[test]
fn version_advisory_uses_known_app_versions_not_protocol_or_package_identity() {
    let mut info = ready_peer();
    info.update.offered = None;
    info.running.version = "1.2.3".into();
    info.protocol.version = "6.2.0".into();
    assert!(version_warning(&info, "1.2.3").is_none());
    info.running.build = Some("99999".into());
    info.running.source_sha = "f".repeat(40);
    assert!(version_warning(&info, "1.2.3").is_none());

    let warning = version_warning(&info, "1.2.4").unwrap();
    assert!(warning.contains("1.2.3"));
    assert!(warning.contains("1.2.4"));
    for unknown in ["unknown", "", "not-a-version"] {
        info.running.version = unknown.into();
        assert!(version_warning(&info, "1.2.4").is_none());
    }
    info.running.version = "1.2.3".into();
    assert!(version_warning(&info, "unknown").is_none());
}

#[gpui::test]
fn version_advisory_does_not_block_the_update_button_or_change_its_exact_target(
    cx: &mut TestAppContext,
) {
    let (screen, cx) = show_update(cx);
    screen.update(cx, |screen, _| seed_update(screen, "b", [2; 16]));
    cx.run_until_parked();
    assert!(cx.debug_bounds("remote-version-warning-a").is_some());
    let update = cx.debug_bounds("update-remote-a").unwrap();
    cx.simulate_click(update.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("remote-update-confirmation").is_some());
    screen.update(cx, |screen, _| {
        let confirmation = screen.install_confirmation.as_ref().unwrap();
        let nonce = confirmation.request.operation_id.clone();
        assert_eq!(confirmation.request.target, target_release());
        assert!(
            confirmation
                .detail
                .contains("will not update or restart automatically")
        );
        assert!(
            screen.computer_updates["a"].attempt.is_none(),
            "review alone never installs"
        );
        let intent = screen.take_install_intent().unwrap();
        assert_eq!(intent.account, "account-a");
        assert_eq!(intent.peer, "a");
        assert_eq!(intent.grant, [1; 16]);
        assert_eq!(intent.request.operation_id, nonce);
        assert_eq!(intent.request.target, target_release());
        assert_eq!(intent.request.target.manifest_sha256, "2".repeat(64));
        assert!(screen.computer_updates["b"].attempt.is_none());
    });
}

#[gpui::test]
fn version_advisory_clears_for_equal_unknown_or_unsupported_peer_versions(cx: &mut TestAppContext) {
    let (screen, cx) = show_update(cx);
    assert!(cx.debug_bounds("remote-version-warning-a").is_some());
    for version in [env!("CARGO_PKG_VERSION"), "unknown"] {
        screen.update(cx, |screen, cx| {
            let mut info = ready_peer();
            info.running.version = version.into();
            info.update.state = UpdateState::Current;
            info.update.offered = None;
            screen.apply_peer_info("a", [1; 16], 1, Ok(Some(info)));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("remote-version-warning-a").is_none());
        screen.read_with(cx, |screen, _| {
            assert!(screen.computer_updates["a"].version_warning.is_none());
            assert!(screen.status.as_ref().unwrap().peers["a"].can_connect);
        });
    }
    screen.update(cx, |screen, cx| {
        screen.apply_peer_info("a", [1; 16], 1, Ok(None));
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("remote-version-warning-a").is_none());
}

#[gpui::test]
fn installing_a_different_target_warns_without_requiring_the_client_to_update(
    cx: &mut TestAppContext,
) {
    let (screen, cx) = show_update(cx);
    let mut target = target_release();
    let mut version = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    version.patch += 1;
    target.version = version.to_string();
    screen.update(cx, |screen, cx| {
        let mut info = ready_peer();
        info.running.version = env!("CARGO_PKG_VERSION").into();
        info.update.offered = Some(target.clone());
        screen.apply_peer_info("a", [1; 16], 1, Ok(Some(info)));
        cx.notify();
    });
    cx.run_until_parked();
    let update = cx.debug_bounds("update-remote-a").unwrap();
    cx.simulate_click(update.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    screen.read_with(cx, |screen, _| {
        let confirmation = screen.install_confirmation.as_ref().unwrap();
        assert_eq!(confirmation.request.target, target);
        assert!(
            confirmation
                .detail
                .contains(&format!("v{}", target.version))
        );
        assert!(
            confirmation
                .detail
                .contains(&format!("v{}", env!("CARGO_PKG_VERSION")))
        );
        assert!(
            confirmation
                .detail
                .contains("will not update or restart automatically")
        );
        assert!(screen.computer_updates["a"].install_available);
        assert!(screen.computer_updates["a"].attempt.is_none());
    });
}

#[gpui::test]
fn local_updater_changes_do_not_cancel_or_retarget_remote_consent(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let backend =
        Arc::new(AgentBackend::for_test("http://127.0.0.1:9".into(), String::new()).unwrap());
    let settings = cx.new(|cx| {
        crate::ui::settings::SettingsScreen::new(
            backend.clone(),
            backend,
            "account-a".into(),
            crate::settings::AppSettings::default(),
            crate::shortcuts::ShortcutSnapshot {
                generation: 1,
                rows: Vec::new(),
                last_error: None,
                compatibility_warning: None,
            },
            crate::ui::settings::Section::General,
            cx,
        )
    });
    let screen = settings.read_with(cx, |settings, _| settings.remote_screen_for_test());
    // Finish constructor refreshes before supplying the authenticated test peer.
    cx.run_until_parked();
    screen.update(cx, |screen, _| {
        screen.status = Some(Status {
            name: "Test Mac".into(),
            enabled: true,
            ready: true,
            error: None,
            endpoint: Some("local".into()),
            peers: Default::default(),
        });
        seed_update(screen, "a", [1; 16]);
    });
    let (_, cx) = cx.add_window_view(|window, cx| {
        window.activate_window();
        crate::desktop::register_key_bindings(cx);
        UpdateHost(screen.clone())
    });
    cx.simulate_resize(gpui::size(px(640.), px(600.)));
    cx.run_until_parked();
    let button = cx.debug_bounds("update-remote-a").unwrap();
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    let request = screen.read_with(cx, |screen, _| {
        screen
            .install_confirmation
            .as_ref()
            .unwrap()
            .request
            .clone()
    });
    for state in [
        crate::update::State::Downloading,
        crate::update::State::Installing,
        crate::update::State::RestartRequired,
        crate::update::State::Disabled,
    ] {
        settings.update(cx, |settings, cx| {
            let mut snapshot = crate::update::Snapshot::initial();
            snapshot.state = state;
            let mut unrelated = target_release();
            unrelated.version = "99.0.0".into();
            snapshot.remote_candidate = Some(unrelated);
            settings.set_update(snapshot, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("remote-update-confirmation").is_some());
        screen.read_with(cx, |screen, _| {
            assert_eq!(
                screen.install_confirmation.as_ref().unwrap().request,
                request
            );
            assert!(screen.computer_updates["a"].attempt.is_none());
        });
    }
    screen.update(cx, |screen, _| {
        let intent = screen.take_install_intent().unwrap();
        assert_eq!(intent.request, request);
        assert_eq!(intent.account, "account-a");
        assert_eq!(intent.grant, [1; 16]);
    });
}

#[gpui::test]
fn password_protected_ready_peer_never_opens_install_confirmation(cx: &mut TestAppContext) {
    let (screen, cx) = show_update(cx);
    screen.update(cx, |screen, _| {
        let update = screen.computer_updates.get_mut("a").unwrap();
        update.info.as_mut().unwrap().update.blocked =
            Some(crate::remote::update::BlockedReason::PasswordProtected);
        update.refresh_presentation();
        assert!(!update.install_available);
        assert!(
            update
                .blocked_label
                .as_ref()
                .unwrap()
                .contains("password protected")
        );
    });
    cx.update(|window, cx| {
        screen.update(cx, |screen, cx| {
            screen.ask_remote_install("a".into(), window, cx)
        })
    });
    screen.read_with(cx, |screen, _| {
        assert!(screen.install_confirmation.is_none());
        assert!(screen.computer_updates["a"].attempt.is_none());
    });
}

#[gpui::test]
fn remote_update_consent_defaults_to_cancel_without_touching_client(cx: &mut TestAppContext) {
    let (screen, cx) = show_update(cx);
    for key in ["enter", "escape"] {
        cx.update(|window, cx| {
            screen.update(cx, |screen, cx| {
                screen.ask_remote_install("a".into(), window, cx)
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("remote-update-confirmation").is_some());
        screen.read_with(cx, |screen, _| {
            let text = &screen.install_confirmation.as_ref().unwrap().detail;
            for required in [
                "Office a",
                "v0.0.7",
                "Remote access may not return",
                "repair Maple directly",
                "other computers",
                "will not update or restart automatically",
            ] {
                assert!(text.contains(required), "missing warning: {required}");
            }
        });
        cx.simulate_keystrokes(key);
        screen.read_with(cx, |screen, _| {
            assert!(screen.install_confirmation.is_none());
            assert!(screen.computer_updates["a"].attempt.is_none());
        });
    }
}

#[gpui::test]
fn remote_update_requires_explicit_install_focus_and_leaves_other_computers_alone(
    cx: &mut TestAppContext,
) {
    let (screen, cx) = show_update(cx);
    screen.update(cx, |screen, _| seed_update(screen, "b", [2; 16]));
    cx.update(|window, cx| {
        screen.update(cx, |screen, cx| {
            screen.ask_remote_install("a".into(), window, cx)
        })
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("tab enter");
    screen.read_with(cx, |screen, _| {
        assert!(screen.install_confirmation.is_none());
        let attempt = screen.computer_updates["a"].attempt.as_ref().unwrap();
        assert_eq!(attempt.request.target, target_release());
        assert!(screen.computer_updates["b"].attempt.is_none());
    });
}

#[gpui::test]
fn old_peer_status_fallback_does_not_remove_ordinary_remote_access(cx: &mut TestAppContext) {
    let screen = screen(cx);
    screen.update(cx, |screen, _| {
        seed_update(screen, "a", [1; 16]);
        screen.apply_peer_info("a", [1; 16], 1, Ok(None));
        let update = &screen.computer_updates["a"];
        assert_eq!(update.phase, PeerInfoPhase::Unsupported);
        assert!(!update.install_available);
        assert!(
            update
                .update_label
                .contains("Open projects for ordinary remote access")
        );
        assert_eq!(screen.peer_grant("a"), Some([1; 16]));
        assert!(screen.status.as_ref().unwrap().peers["a"].can_connect);
    });
}

#[gpui::test]
fn confirmed_intent_keeps_peer_grant_account_and_release_after_selection_changes(
    cx: &mut TestAppContext,
) {
    let (screen, cx) = show_update(cx);
    screen.update(cx, |screen, _| seed_update(screen, "b", [2; 16]));
    cx.update(|window, cx| {
        screen.update(cx, |screen, cx| {
            screen.ask_remote_install("a".into(), window, cx)
        })
    });
    screen.update(cx, |screen, _| {
        let nonce = screen
            .install_confirmation
            .as_ref()
            .unwrap()
            .request
            .operation_id
            .clone();
        screen.selected_device = Some("b".into());
        let intent = screen.take_install_intent().unwrap();
        assert_eq!(intent.account, "account-a");
        assert_eq!(intent.peer, "a");
        assert_eq!(intent.grant, [1; 16]);
        assert_eq!(intent.request.operation_id, nonce);
        assert_eq!(intent.request.target, target_release());
        assert!(screen.computer_updates["b"].attempt.is_none());
    });
}

#[gpui::test]
fn stale_grant_or_offer_invalidates_remote_update_consent(cx: &mut TestAppContext) {
    let (screen, cx) = show_update(cx);
    cx.update(|window, cx| {
        screen.update(cx, |screen, cx| {
            screen.ask_remote_install("a".into(), window, cx)
        })
    });
    screen.update(cx, |screen, _| {
        screen
            .status
            .as_mut()
            .unwrap()
            .peers
            .get_mut("a")
            .unwrap()
            .grant = [2; 16];
        assert!(
            screen.take_install_intent().is_none(),
            "submission rechecks pairing even before a refresh callback"
        );
        assert!(screen.computer_updates["a"].attempt.is_none());
        screen
            .status
            .as_mut()
            .unwrap()
            .peers
            .get_mut("a")
            .unwrap()
            .grant = [1; 16];
    });
    cx.update(|window, cx| {
        screen.update(cx, |screen, cx| {
            screen.ask_remote_install("a".into(), window, cx)
        })
    });
    screen.update(cx, |screen, _| {
        let mut replacement = ready_peer();
        replacement.update.offered.as_mut().unwrap().version = "0.0.8".into();
        screen.apply_peer_info("a", [1; 16], 1, Ok(Some(replacement)));
        assert!(screen.install_confirmation.is_none());
        assert!(screen.take_install_intent().is_none());
        assert!(screen.computer_updates["a"].attempt.is_none());
    });
}

#[gpui::test]
fn old_peer_probe_cannot_replace_newer_generation_or_pairing(cx: &mut TestAppContext) {
    let screen = screen(cx);
    screen.update(cx, |screen, cx| {
        seed_update(screen, "a", [1; 16]);
        screen.computer_updates.get_mut("a").unwrap().generation = 2;
        screen.apply_peer_info("a", [1; 16], 1, Ok(None));
        assert_eq!(screen.computer_updates["a"].phase, PeerInfoPhase::Loaded);
        let mut status = screen.status.clone().unwrap();
        status.peers.get_mut("a").unwrap().grant = [2; 16];
        screen.update_status(status, cx);
        screen.apply_peer_info("a", [1; 16], 2, Ok(Some(ready_peer())));
        assert!(!screen.computer_updates.contains_key("a"));
        assert!(screen.install_confirmation.is_none());
    });
}

#[gpui::test]
fn lost_install_ack_keeps_nonce_and_never_replays_or_updates_client(cx: &mut TestAppContext) {
    let screen = screen(cx);
    screen.update(cx, |screen, _| {
        seed_update(screen, "a", [1; 16]);
        seed_update(screen, "b", [2; 16]);
        let request = InstallRequest {
            operation_id: uuid::Uuid::new_v4().to_string(),
            target: target_release(),
        };
        screen.computer_updates.get_mut("a").unwrap().attempt = Some(InstallAttempt {
            request: request.clone(),
            phase: InstallPhase::Sending,
            label: String::new(),
        });
        screen.apply_install_reply("a", [1; 16], &request, Err("Disconnected".into()));
        screen.apply_peer_info("a", [1; 16], 1, Ok(Some(ready_peer())));
        let attempt = screen.computer_updates["a"].attempt.as_ref().unwrap();
        assert_eq!(attempt.phase, InstallPhase::Uncertain);
        assert_eq!(attempt.request, request);
        assert_eq!(attempt.label, UNCONFIRMED_INSTALL);
        assert!(!screen.computer_updates["a"].install_available);

        let mut acknowledged = ready_peer();
        acknowledged.update.operation = Some(crate::remote::update::RemoteUpdateOperation {
            operation_id: request.operation_id.clone(),
            target: request.target.clone(),
            state: OperationState::Queued,
            message: None,
        });
        screen.apply_peer_info("a", [1; 16], 1, Ok(Some(acknowledged)));
        assert_eq!(
            screen.computer_updates["a"].attempt.as_ref().unwrap().phase,
            InstallPhase::Accepted
        );
        screen.apply_install_reply("a", [1; 16], &request, Err("Late lost reply".into()));
        assert_eq!(
            screen.computer_updates["a"].attempt.as_ref().unwrap().phase,
            InstallPhase::Accepted,
            "a late callback cannot undo an authoritative operation receipt"
        );
        assert!(!screen.computer_updates["a"].install_available);
        assert!(screen.computer_updates["b"].attempt.is_none());
    });
}

#[gpui::test]
fn settings_reentry_adopts_pending_operation_without_enabling_another_install(
    cx: &mut TestAppContext,
) {
    let screen = screen(cx);
    screen.update(cx, |screen, _| {
        seed_update(screen, "a", [1; 16]);
        let operation_id = uuid::Uuid::new_v4().to_string();
        for state in [
            OperationState::PendingAck,
            OperationState::Queued,
            OperationState::Installing,
            OperationState::Unknown,
        ] {
            screen.computer_updates.get_mut("a").unwrap().attempt = None;
            let mut info = ready_peer();
            info.update.operation = Some(crate::remote::update::RemoteUpdateOperation {
                operation_id: operation_id.clone(),
                target: target_release(),
                state,
                message: None,
            });
            screen.apply_peer_info("a", [1; 16], 1, Ok(Some(info)));
            let update = &screen.computer_updates["a"];
            assert!(!update.install_available);
            assert!(
                update
                    .blocked_label
                    .as_ref()
                    .unwrap()
                    .contains("already pending")
            );
            let attempt = update.attempt.as_ref().unwrap();
            assert_eq!(attempt.request.operation_id, operation_id);
            if state == OperationState::PendingAck {
                assert!(attempt.label.contains("Installation has not started"));
            }
        }
    });
}

fn historical_info(state: Option<OperationState>) -> PeerInfo {
    let mut info = ready_peer();
    info.update.state = UpdateState::Current;
    info.update.offered = None;
    info.update.operation = state.map(|state| crate::remote::update::RemoteUpdateOperation {
        operation_id: "11111111-1111-4111-8111-111111111111".into(),
        target: target_release(),
        state,
        message: None,
    });
    info
}

fn historical_attempt(phase: InstallPhase) -> InstallAttempt {
    InstallAttempt {
        request: InstallRequest {
            operation_id: "11111111-1111-4111-8111-111111111111".into(),
            target: target_release(),
        },
        phase,
        label: UNCONFIRMED_INSTALL.into(),
    }
}

#[test]
fn current_and_idle_without_offers_do_not_show_download_instructions() {
    for state in [
        UpdateState::Current,
        UpdateState::Idle,
        UpdateState::Checking,
    ] {
        for blocked in [
            None,
            Some(crate::remote::update::BlockedReason::UpdateNotReady),
        ] {
            let mut info = historical_info(None);
            info.update.state = state;
            info.update.blocked = blocked;
            let mut update = ComputerUpdate {
                phase: PeerInfoPhase::Loaded,
                info: Some(info),
                ..Default::default()
            };
            update.refresh_presentation();
            assert_eq!(update.update_label, state.label());
            assert!(update.blocked_label.is_none());
            assert!(
                !update.install_available,
                "hiding an irrelevant explanation must not enable installation"
            );
        }
    }
    for reason in [
        crate::remote::update::BlockedReason::PasswordProtected,
        crate::remote::update::BlockedReason::DesktopUnavailable,
        crate::remote::update::BlockedReason::CoordinatorUnavailable,
        crate::remote::update::BlockedReason::Busy,
    ] {
        let mut info = historical_info(None);
        info.update.blocked = Some(reason);
        let mut update = ComputerUpdate {
            phase: PeerInfoPhase::Loaded,
            info: Some(info),
            ..Default::default()
        };
        update.refresh_presentation();
        assert_eq!(update.blocked_label.as_deref(), Some(reason.message()));
        assert!(!update.install_available);
    }
}

#[test]
fn confirmed_remote_outcome_stays_historical_after_later_upgrade() {
    let mut update = ComputerUpdate {
        phase: PeerInfoPhase::Loaded,
        info: Some(historical_info(Some(OperationState::Succeeded))),
        ..Default::default()
    };
    update.reconcile_operation();
    let confirmed = update.attempt.as_ref().unwrap().label.clone();
    assert!(confirmed.contains("requested v0.0.7 was running"));
    assert!(confirmed.contains("earlier update request"));
    assert!(!confirmed.contains("now running"));
    for status in [None, Some(OperationState::Failed)] {
        let mut later = historical_info(status);
        later.running.version = "0.0.8".into();
        later.running.source_sha = "f".repeat(40);
        later.running.build = Some("552".into());
        update.info = Some(later);
        update.reconcile_operation();
        update.refresh_presentation();
        let attempt = update.attempt.as_ref().unwrap();
        assert_eq!(attempt.phase, InstallPhase::Succeeded);
        assert_eq!(attempt.label, confirmed);
        assert_eq!(update.update_label, "Up to date");
        assert!(update.blocked_label.is_none());
        assert!(!update.install_available);
    }
}

#[test]
fn running_target_or_later_version_does_not_prove_an_old_request_succeeded() {
    for phase in [InstallPhase::Uncertain, InstallPhase::Failed] {
        for version in ["0.0.7", "0.0.8"] {
            for state in [
                None,
                Some(OperationState::Failed),
                Some(OperationState::Unknown),
            ] {
                let mut info = historical_info(state);
                info.running.version = version.into();
                info.running.source_sha = if version == "0.0.7" { "1" } else { "f" }.repeat(40);
                info.running.build = Some(if version == "0.0.7" { "551" } else { "552" }.into());
                let mut update = ComputerUpdate {
                    phase: PeerInfoPhase::Loaded,
                    info: Some(info),
                    attempt: Some(historical_attempt(phase)),
                    ..Default::default()
                };
                update.reconcile_operation();
                let attempt = update.attempt.as_ref().unwrap();
                assert_ne!(attempt.phase, InstallPhase::Succeeded);
                assert!(!attempt.label.contains("completed"));
                assert!(!attempt.label.contains("now running"));
            }
        }
    }
}

#[test]
fn only_matching_success_receipt_can_finish_a_nonterminal_request() {
    for phase in [InstallPhase::Accepted, InstallPhase::Uncertain] {
        let mut wrong = historical_info(Some(OperationState::Succeeded));
        wrong.update.operation.as_mut().unwrap().operation_id =
            "22222222-2222-4222-8222-222222222222".into();
        let mut update = ComputerUpdate {
            phase: PeerInfoPhase::Loaded,
            info: Some(wrong),
            attempt: Some(historical_attempt(phase)),
            ..Default::default()
        };
        update.reconcile_operation();
        assert_eq!(update.attempt.as_ref().unwrap().phase, phase);
        let mut matching = historical_info(Some(OperationState::Succeeded));
        matching.running.version = "0.0.8".into();
        matching.running.source_sha = "f".repeat(40);
        update.info = Some(matching);
        update.reconcile_operation();
        assert_eq!(
            update.attempt.as_ref().unwrap().phase,
            InstallPhase::Succeeded
        );
    }
}
