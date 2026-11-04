use super::*;
use gpui::Focusable;

// Public synthetic values only. These tests never authenticate, contact a host,
// or open the user's profile; AgentBackend::for_test owns its disposable store.
const STRONG: &str = "Z7jv!9qP#3nMx@8Ks^2dH";
const CURRENT: &str = "synthetic current unlock password";

fn form(cx: &mut gpui::TestAppContext, action: PasswordAction) -> Entity<PasswordForm> {
    cx.executor().allow_parking();
    let backend =
        Arc::new(AgentBackend::for_test("http://127.0.0.1:1".into(), String::new()).unwrap());
    cx.new(|cx| PasswordForm::new(backend, action, Vec::new(), cx))
}

fn set_secret(input: &Entity<SecretInput>, text: &str, cx: &mut Context<PasswordForm>) {
    input.update(cx, |input, cx| {
        assert!(input.replace_secret(Zeroizing::new(text.to_owned()), cx));
    });
}

fn fill_new_password(form: &mut PasswordForm, confirmation: &str, cx: &mut Context<PasswordForm>) {
    set_secret(&form.password, STRONG, cx);
    set_secret(&form.confirmation, confirmation, cx);
    form.assess(cx);
}

fn assert_no_submission(form: &PasswordForm) {
    assert!(!form.busy);
    assert!(form.bridges.borrow().is_empty());
    assert!(form.progress_task.is_none());
}

#[gpui::test]
fn strong_matching_change_explains_missing_current_and_preserves_secrets(
    cx: &mut gpui::TestAppContext,
) {
    let entity = form(cx, PasswordAction::Change);
    entity.update(cx, |form, cx| {
        fill_new_password(form, STRONG, cx);
        assert_eq!(form.assessment.unwrap().level, StrengthLevel::Strong);
        assert!(!form.can_submit(cx));
        let reason = form
            .validation_message(cx)
            .expect("missing-current feedback");
        assert!(reason.to_lowercase().contains("current"));

        form.submit(cx);
        assert_eq!(form.message.as_deref(), Some(reason.as_ref()));
        assert!(form.current.read(cx).is_empty());
        form.password
            .read(cx)
            .with_secret(|value| assert_eq!(value, STRONG));
        form.confirmation
            .read(cx)
            .with_secret(|value| assert_eq!(value, STRONG));
        assert_no_submission(form);
    });
}

#[gpui::test]
fn mismatch_has_specific_feedback_and_does_not_consume_any_field(cx: &mut gpui::TestAppContext) {
    let entity = form(cx, PasswordAction::Change);
    entity.update(cx, |form, cx| {
        set_secret(&form.current, CURRENT, cx);
        fill_new_password(form, "different synthetic confirmation", cx);
        let reason = form.validation_message(cx).expect("mismatch feedback");
        assert!(reason.to_lowercase().contains("match"));
        assert!(!form.can_submit(cx));

        form.submit(cx);
        assert_eq!(form.message.as_deref(), Some(reason.as_ref()));
        form.current
            .read(cx)
            .with_secret(|value| assert_eq!(value, CURRENT));
        form.password
            .read(cx)
            .with_secret(|value| assert_eq!(value, STRONG));
        form.confirmation.read(cx).with_secret(|value| {
            assert_eq!(value, "different synthetic confirmation");
        });
        assert_no_submission(form);
    });
}

#[gpui::test]
fn strong_password_requires_confirmation_with_an_explicit_reason(cx: &mut gpui::TestAppContext) {
    let entity = form(cx, PasswordAction::Set);
    entity.update(cx, |form, cx| {
        set_secret(&form.password, STRONG, cx);
        form.assess(cx);
        assert_eq!(form.assessment.unwrap().level, StrengthLevel::Strong);
        let reason = form
            .validation_message(cx)
            .expect("confirmation-required feedback");
        assert!(reason.to_lowercase().contains("confirm"));
        form.submit(cx);
        assert_eq!(form.message.as_deref(), Some(reason.as_ref()));
        assert_no_submission(form);
    });
}

#[gpui::test]
fn weak_password_reports_strength_instead_of_silently_ignoring_save(cx: &mut gpui::TestAppContext) {
    let entity = form(cx, PasswordAction::Set);
    entity.update(cx, |form, cx| {
        set_secret(&form.password, "passwordpassword", cx);
        set_secret(&form.confirmation, "passwordpassword", cx);
        form.assess(cx);
        assert_eq!(form.assessment.unwrap().level, StrengthLevel::TooEasy);
        let reason = form.validation_message(cx).expect("weak-password feedback");
        let lower = reason.to_lowercase();
        assert!(
            lower.contains("strong")
                || lower.contains("easy")
                || lower.contains("predictable")
                || lower.contains("guess")
        );
        form.submit(cx);
        assert_eq!(form.message.as_deref(), Some(reason.as_ref()));
        assert_no_submission(form);
    });
}

#[gpui::test]
fn an_overlength_password_reports_its_bound_before_confirmation(cx: &mut gpui::TestAppContext) {
    let entity = form(cx, PasswordAction::Set);
    entity.update(cx, |form, cx| {
        let too_long = "x".repeat(1025);
        set_secret(&form.password, &too_long, cx);
        set_secret(&form.confirmation, &too_long, cx);
        form.assess(cx);
        let reason = form
            .validation_message(cx)
            .expect("password-length feedback");
        let lower = reason.to_lowercase();
        assert!(lower.contains("long") || lower.contains("1024") || lower.contains("1,024"));
        form.submit(cx);
        assert_eq!(form.message.as_deref(), Some(reason.as_ref()));
        assert_eq!(form.password.read(cx).len_chars(), 1025);
        assert_no_submission(form);
    });
}

#[gpui::test]
fn opening_a_password_form_starts_typed_without_a_generator_request(cx: &mut gpui::TestAppContext) {
    for action in [
        PasswordAction::Set,
        PasswordAction::Change,
        PasswordAction::Setup,
    ] {
        let entity = form(cx, action);
        entity.update(cx, |form, cx| {
            assert!(!form.suggested);
            assert!(!form.generating);
            assert!(form.phrase.is_none());
            assert!(form.assessment.is_none());
            assert!(!form.can_submit(cx));
            assert_no_submission(form);
        });
    }
}

#[gpui::test]
fn a_complete_typed_change_is_ready_with_either_reencryption_choice(cx: &mut gpui::TestAppContext) {
    let entity = form(cx, PasswordAction::Change);
    entity.update(cx, |form, cx| {
        set_secret(&form.current, CURRENT, cx);
        fill_new_password(form, STRONG, cx);
        assert!(form.validation_message(cx).is_none());
        assert!(form.can_submit(cx));
        assert!(!form.reencrypt);
        form.reencrypt = true;
        assert!(form.can_submit(cx));
        assert_no_submission(form);
    });
}

#[gpui::test]
fn an_unused_inflight_suggestion_does_not_block_a_complete_typed_change(
    cx: &mut gpui::TestAppContext,
) {
    let entity = form(cx, PasswordAction::Change);
    entity.update(cx, |form, cx| {
        set_secret(&form.current, CURRENT, cx);
        fill_new_password(form, STRONG, cx);
        // Model switching to typed mode before an earlier suggestion resolves.
        form.generating = true;
        assert!(!form.suggested);
        assert!(form.validation_message(cx).is_none());
        assert!(form.can_submit(cx));
        form.generating = false;
        assert!(form.can_submit(cx));
        assert_no_submission(form);
    });
}

struct PasswordFormHost(Entity<PasswordForm>);

impl Render for PasswordFormHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().p_6().child(self.0.clone())
    }
}

#[gpui::test]
fn empty_change_password_fields_have_visible_ordered_input_frames(cx: &mut gpui::TestAppContext) {
    let entity = form(cx, PasswordAction::Change);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        window.activate_window();
        crate::desktop::register_key_bindings(cx);
        PasswordFormHost(entity.clone())
    });
    cx.simulate_resize(gpui::size(px(900.), px(1100.)));
    cx.run_until_parked();

    let current = cx
        .debug_bounds("password-current-frame")
        .expect("current password frame");
    let new = cx
        .debug_bounds("password-new-frame")
        .expect("new password frame");
    let confirmation = cx
        .debug_bounds("password-confirmation-frame")
        .expect("confirmation frame");
    for frame in [current, new, confirmation] {
        assert!(
            frame.size.height >= px(32.),
            "an empty password field must remain visible"
        );
        assert!(frame.size.width >= px(200.));
        assert!(frame.origin.x >= px(0.));
        assert!(frame.origin.y >= px(0.));
        assert!(frame.bottom() <= px(1100.));
    }
    assert!(current.bottom() < new.origin.y);
    assert!(new.bottom() < confirmation.origin.y);
    cx.update(|_window, app| {
        let form = entity.read(app);
        assert!(form.current.read(app).is_empty());
        assert!(form.password.read(app).is_empty());
        assert!(form.confirmation.read(app).is_empty());
        assert_no_submission(form);
    });
}

#[gpui::test]
fn change_keyboard_navigation_and_save_explain_missing_current_without_consuming_secrets(
    cx: &mut gpui::TestAppContext,
) {
    let entity = form(cx, PasswordAction::Change);
    let (_host, cx) = cx.add_window_view(|window, cx| {
        window.activate_window();
        crate::desktop::register_key_bindings(cx);
        PasswordFormHost(entity.clone())
    });
    cx.simulate_resize(gpui::size(px(900.), px(1100.)));
    cx.run_until_parked();
    let (current_focus, new_focus, confirmation_focus) = cx.update(|_window, app| {
        let form = entity.read(app);
        (
            form.current.read(app).focus_handle(app),
            form.password.read(app).focus_handle(app),
            form.confirmation.read(app).focus_handle(app),
        )
    });

    cx.update(|window, app| window.focus(&current_focus, app));
    cx.simulate_keystrokes("tab");
    cx.update(|window, _app| assert!(new_focus.is_focused(window)));
    cx.simulate_input(STRONG);
    cx.simulate_keystrokes("tab");
    cx.update(|window, _app| assert!(confirmation_focus.is_focused(window)));
    cx.simulate_input(STRONG);
    cx.simulate_keystrokes("tab");
    cx.update(|window, app| {
        assert!(!current_focus.is_focused(window));
        assert!(!new_focus.is_focused(window));
        assert!(!confirmation_focus.is_focused(window));
        assert!(!entity.read(app).reencrypt);
    });
    cx.simulate_keystrokes("space");
    cx.update(|_window, app| {
        assert!(
            entity.read(app).reencrypt,
            "Space must activate the checkbox after Tab"
        );
    });
    cx.simulate_keystrokes("shift-tab");
    cx.update(|window, _app| assert!(confirmation_focus.is_focused(window)));
    cx.simulate_keystrokes("enter");
    cx.update(|_window, app| {
        let form = entity.read(app);
        assert_eq!(form.assessment.unwrap().level, StrengthLevel::Strong);
        assert!(form.confirmation_matches);
        assert!(
            form.message
                .as_deref()
                .is_some_and(|message| message.contains("current"))
        );
        assert!(form.current.read(app).is_empty());
        form.password
            .read(app)
            .with_secret(|value| assert_eq!(value, STRONG));
        form.confirmation
            .read(app)
            .with_secret(|value| assert_eq!(value, STRONG));
        assert_no_submission(form);
    });
    let feedback = cx
        .debug_bounds("store-password-feedback")
        .expect("visible Enter feedback");
    assert!(feedback.size.height > px(0.));

    // A second attempt through the actual Save button must explain the same
    // unmet requirement, rather than swallowing the click or starting an RPC.
    entity.update(cx, |form, cx| {
        form.message = None;
        cx.notify();
    });
    cx.run_until_parked();
    let save = cx
        .debug_bounds("store-password-submit")
        .expect("Save button");
    cx.simulate_click(save.center(), gpui::Modifiers::default());
    cx.update(|_window, app| {
        let form = entity.read(app);
        assert!(
            form.message
                .as_deref()
                .is_some_and(|message| message.contains("current"))
        );
        assert!(form.current.read(app).is_empty());
        form.password
            .read(app)
            .with_secret(|value| assert_eq!(value, STRONG));
        form.confirmation
            .read(app)
            .with_secret(|value| assert_eq!(value, STRONG));
        assert_no_submission(form);
    });
    assert!(cx.debug_bounds("store-password-feedback").is_some());
}

fn install_synthetic_suggestion(form: &mut PasswordForm, cx: &mut Context<PasswordForm>) {
    form.suggested = true;
    form.phrase = Some(SuggestedPhrase {
        id: "synthetic-host-receipt".into(),
        passphrase: SecretString::new("synthetic amber basket orbit sample fixture".into()),
        assessment: PasswordAssessment {
            level: StrengthLevel::Strong,
            reason: keys::StrengthReason::Accepted,
            guesses_log10: 20.,
        },
    });
    form.assess(cx);
}

#[gpui::test]
fn generated_phrase_survives_submission_and_proven_precommit_failure(
    cx: &mut gpui::TestAppContext,
) {
    let entity = form(cx, PasswordAction::Set);
    entity.update(cx, |form, cx| {
        install_synthetic_suggestion(form, cx);
        set_secret(
            &form.confirmation,
            "synthetic amber basket orbit sample fixture",
            cx,
        );
        form.assess(cx);
        assert!(form.can_submit(cx));
        form.submit(cx);
        assert!(
            form.phrase.is_some(),
            "sending a request must not consume the saved suggestion"
        );
        assert_eq!(form.phrase.as_ref().unwrap().id, "synthetic-host-receipt");
        form.submission_completed(
            Ok(PasswordAttempt {
                result: Err("Synthetic precommit rejection".into()),
                generated_retry_valid: true,
            }),
            cx,
        );
        assert!(!form.suggestion_retry_blocked);
        assert_eq!(form.assessment.unwrap().level, StrengthLevel::Strong);
        assert!(form.phrase.is_some());
        set_secret(
            &form.confirmation,
            "synthetic amber basket orbit sample fixture",
            cx,
        );
        form.assess(cx);
        assert!(
            form.can_submit(cx),
            "a proven unused receipt permits an explicit retry"
        );
    });
}

#[gpui::test]
fn an_uncertain_generated_attempt_retains_the_phrase_but_blocks_resubmission(
    cx: &mut gpui::TestAppContext,
) {
    let entity = form(cx, PasswordAction::Set);
    entity.update(cx, |form, cx| {
        install_synthetic_suggestion(form, cx);
        form.busy = true;
        form.submission_completed(
            Ok(PasswordAttempt {
                result: Err("Synthetic unacknowledged outcome".into()),
                generated_retry_valid: false,
            }),
            cx,
        );
        assert!(form.suggestion_retry_blocked);
        assert!(
            form.message
                .as_deref()
                .unwrap()
                .contains("Keep this passphrase")
        );
        assert_eq!(
            form.phrase.as_ref().unwrap().passphrase.expose(),
            "synthetic amber basket orbit sample fixture"
        );
        set_secret(
            &form.confirmation,
            "synthetic amber basket orbit sample fixture",
            cx,
        );
        form.assess(cx);
        assert!(!form.can_submit(cx));
        form.submit(cx);
        assert_no_submission(form);
        assert!(form.phrase.is_some());
    });
}

struct StoreGateHost(Entity<StoreGate>);

impl Render for StoreGateHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

#[gpui::test]
fn locked_gate_tab_space_and_enter_reach_erase_confirmation_and_cancel(
    cx: &mut gpui::TestAppContext,
) {
    cx.executor().allow_parking();
    let backend =
        Arc::new(AgentBackend::for_test("http://127.0.0.1:1".into(), String::new()).unwrap());
    let gate = cx.new(|cx| StoreGate::new_mounted(backend, StoreState::Locked, cx));
    let (_host, cx) = cx.add_window_view(|window, cx| {
        window.activate_window();
        crate::desktop::register_key_bindings(cx);
        StoreGateHost(gate.clone())
    });
    cx.simulate_resize(gpui::size(px(900.), px(900.)));
    cx.run_until_parked();
    let unlock_focus = cx.update(|_, app| gate.read(app).password.read(app).focus_handle(app));
    cx.update(|window, app| window.focus(&unlock_focus, app));
    cx.simulate_keystrokes("enter");
    cx.update(|_, app| {
        let gate = gate.read(app);
        assert!(!gate.busy);
        assert!(gate.bridges.borrow().is_empty());
        assert!(
            gate.message
                .as_ref()
                .unwrap()
                .resolve()
                .contains("unlock password"),
            "unexpected unlock feedback: {:?}",
            gate.message
        );
    });
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("space");
    cx.update(|window, app| {
        let gate = gate.read(app);
        assert!(
            gate.erase,
            "Space on Forgot password must open the confirmation"
        );
        assert!(
            gate.confirmation
                .read(app)
                .focus_handle(app)
                .is_focused(window)
        );
    });
    cx.simulate_input("erase");
    cx.simulate_keystrokes("enter");
    cx.update(|_, app| {
        let gate = gate.read(app);
        assert!(!gate.busy);
        assert!(
            gate.message
                .as_ref()
                .unwrap()
                .resolve()
                .contains("Type ERASE exactly")
        );
        assert_eq!(gate.confirmation.read(app).text(), "erase");
    });
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("enter");
    cx.update(|_, app| {
        let gate = gate.read(app);
        assert!(!gate.busy);
        assert!(
            gate.message
                .as_ref()
                .unwrap()
                .resolve()
                .contains("Type ERASE exactly")
        );
    });
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("space");
    cx.update(|window, app| {
        let gate = gate.read(app);
        assert!(!gate.erase, "Space on Cancel must return to unlock");
        assert_eq!(gate.state, StoreState::Locked);
        assert!(gate.confirmation.read(app).text().is_empty());
        assert!(unlock_focus.is_focused(window));
        assert!(!gate.busy);
    });
}

#[gpui::test]
fn needs_setup_gate_keyboard_opens_the_typed_password_form(cx: &mut gpui::TestAppContext) {
    cx.executor().allow_parking();
    let backend =
        Arc::new(AgentBackend::for_test("http://127.0.0.1:1".into(), String::new()).unwrap());
    let gate = cx.new(|cx| StoreGate::new_mounted(backend, StoreState::NeedsSetup, cx));
    gate.update(cx, |gate, _cx| {
        gate.status = Some(serde_json::from_value(serde_json::json!({
            "state":{"state":"needs_setup"}, "mode":"password", "password_settings_available":true
        })).unwrap());
    });
    let (_host, cx) = cx.add_window_view(|window, cx| {
        window.activate_window();
        crate::desktop::register_key_bindings(cx);
        StoreGateHost(gate.clone())
    });
    cx.simulate_resize(gpui::size(px(900.), px(900.)));
    cx.run_until_parked();
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("space");
    cx.update(|_, app| {
        let gate = gate.read(app);
        let form = gate
            .form
            .as_ref()
            .expect("keyboard activation must open password setup")
            .read(app);
        assert!(form.action == PasswordAction::Setup);
        assert!(!form.suggested);
        assert!(!form.generating);
        assert!(!form.busy);
    });
}
