//! Sign-in screen: email and password, or OAuth (GitHub, Google, Apple)
//! against the OpenSecret backend.

use std::sync::Arc;

use gpui::{AppContext, Context, Div, Entity, EventEmitter, Render, Window, div, prelude::*};

use crate::backend::{AgentBackend, AuthSession, OAuthProvider};
use crate::ui::icons::wordmark;
use crate::ui::text_input::TextInput;
use crate::ui::theme;
use crate::ui::widgets;

/// Emitted after the backend validated the credentials. Only the account id
/// crosses the event boundary; the token snapshot stays inside the backend.
pub struct LoginSucceeded(pub String);

/// How the OAuth completion step is presented.
enum OAuthFlow {
    Idle,
    Pending {
        provider: OAuthProvider,
        auth_url: String,
    },
}

pub struct LoginScreen {
    backend: Arc<AgentBackend>,
    email_input: Entity<TextInput>,
    password_input: Entity<TextInput>,
    /// Paste field for the OAuth redirect URL.
    callback_input: Entity<TextInput>,
    oauth: OAuthFlow,
    /// Backend-call bridges retained for thread-affinity; see
    /// [`crate::ui::task::call`].
    bridged_tasks: std::cell::RefCell<Vec<gpui::Task<()>>>,
    error: Option<String>,
    busy: bool,
}

impl LoginScreen {
    pub fn new(backend: Arc<AgentBackend>, cx: &mut Context<Self>) -> Self {
        let email = cx.new(|cx| TextInput::new("Email", cx).with_tab_index(0));
        let password = cx.new(|cx| TextInput::new("Password", cx).masked().with_tab_index(1));
        let callback = cx.new(|cx| {
            TextInput::new("Paste the URL you were redirected to…", cx).with_tab_index(0)
        });
        // Enter handlers receive their own field's text and read the sibling
        // through its entity; neither path leases the focused input.
        let (email_handle, password_handle) = (email.clone(), password.clone());
        let weak = cx.entity().downgrade();
        let email_weak = weak.clone();
        let password_weak = weak.clone();
        email.update(cx, |input, _| {
            input.set_on_enter(move |email_text, _, cx| {
                let password_text = password_handle.read(cx).text();
                if let Some(this) = email_weak.upgrade() {
                    this.update(cx, |screen, cx| {
                        screen.submit_values(email_text, password_text, cx)
                    });
                }
            });
        });
        password.update(cx, |input, _| {
            input.set_on_enter(move |password_text, _, cx| {
                let email_text = email_handle.read(cx).text();
                if let Some(this) = password_weak.upgrade() {
                    this.update(cx, |screen, cx| {
                        screen.submit_values(email_text, password_text, cx)
                    });
                }
            });
        });
        let oauth_weak = weak;
        callback.update(cx, |input, _| {
            input.set_on_enter(move |_, _, cx| {
                if let Some(this) = oauth_weak.upgrade() {
                    this.update(cx, |screen, cx| screen.confirm_oauth(cx));
                }
            });
        });
        Self {
            bridged_tasks: std::cell::RefCell::new(Vec::new()),
            backend,
            email_input: email,
            password_input: password,
            callback_input: callback,
            oauth: OAuthFlow::Idle,
            error: None,
            busy: false,
        }
    }

    fn submit_values(&mut self, email: String, password: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if email.trim().is_empty() || password.is_empty() {
            self.error = Some("Enter your email and password".to_string());
            cx.notify();
            return;
        }
        let backend = self.begin(cx);
        self.call(
            async move { backend.login(email, password).await },
            cx,
            |this, result, cx| match result {
                Ok(session) => cx.emit(LoginSucceeded(session.user_id)),
                // The backend already sanitizes its own error strings.
                Err(message) => this.error = Some(message),
            },
        );
    }

    /// Enter the busy state for a backend call and hand back the backend
    /// for the future to own.
    fn begin(&mut self, cx: &mut Context<Self>) -> Arc<AgentBackend> {
        self.busy = true;
        self.error = None;
        cx.notify();
        self.backend.clone()
    }

    /// Run a backend call; `then` runs after the busy state is cleared.
    fn call<T, F>(
        &self,
        future: F,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    ) where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    {
        // Retained for the same thread-affinity reason as ChatScreen's
        // bridges; see ui::task::call.
        let bridge = crate::ui::task::call(&self.backend, future, cx, |this, result, cx| {
            this.busy = false;
            then(this, result, cx);
            cx.notify();
        });
        crate::ui::task::retain(&self.bridged_tasks, bridge);
    }

    fn submit_clicked(
        &mut self,
        _event: &gpui::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let email = self.email_input.read(cx).text();
        let password = self.password_input.read(cx).text();
        self.submit_values(email, password, cx);
    }

    fn start_oauth(&mut self, provider: OAuthProvider, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let backend = self.begin(cx);
        self.call(
            async move { backend.oauth_start(provider).await },
            cx,
            move |this, result, _cx| match result {
                Ok(auth_url) => this.oauth = OAuthFlow::Pending { provider, auth_url },
                Err(message) => this.error = Some(message),
            },
        );
    }

    fn confirm_oauth(&mut self, cx: &mut Context<Self>) {
        let OAuthFlow::Pending { provider, .. } = self.oauth else {
            return;
        };
        if self.busy {
            return;
        }
        let redirected = self.callback_input.read(cx).text();
        if redirected.trim().is_empty() {
            self.error = Some("Paste the URL you were redirected to".to_string());
            cx.notify();
            return;
        }
        let backend = self.begin(cx);
        self.call(
            async move { backend.oauth_complete(provider, redirected).await },
            cx,
            Self::oauth_completed,
        );
    }

    fn oauth_completed(&mut self, result: Result<AuthSession, String>, cx: &mut Context<Self>) {
        match result {
            // Success means native authentication is already installed and
            // persisted. If Back raced with its queued UI receipt, follow
            // that committed account instead of leaving a signed-out form
            // over a live session. `busy` prevents another sign-in until
            // this completion is delivered.
            Ok(session) => cx.emit(LoginSucceeded(session.user_id)),
            Err(_) if matches!(self.oauth, OAuthFlow::Idle) => {}
            Err(message) => self.error = Some(message),
        }
    }

    fn cancel_oauth(&mut self, cx: &mut Context<Self>) {
        self.backend.cancel_oauth();
        self.oauth = OAuthFlow::Idle;
        self.error = None;
        self.callback_input.update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }
}

impl EventEmitter<LoginSucceeded> for LoginScreen {}

impl Render for LoginScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.busy;
        let mut card = div()
            .flex()
            .flex_col()
            .gap_3()
            .w(gpui::px(380.))
            .p_6()
            .rounded(theme::RADIUS_XL)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .when(busy, |container| container.opacity(0.7))
            .tab_group()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(wordmark(gpui::px(22.), theme::text_primary()))
                    .child(div()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_secondary()))
                    .child(format!("Sign in to {}", self.backend.api_url())),
            );

        match &self.oauth {
            OAuthFlow::Idle => {
                card = card
                    .child(field("Email", self.email_input.clone()))
                    .child(field("Password", self.password_input.clone()))
                    .child(
                        widgets::primary_button("login-submit")
                            .w_full()
                            .mt_1()
                            .when(busy, |el| el.bg(gpui::rgb(theme::bg_sidebar_pill())))
                            .when(!busy, |el| el.on_click(cx.listener(Self::submit_clicked)))
                            .child(if busy {
                                "Signing in…".to_string()
                            } else {
                                "Sign in".to_string()
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .h(gpui::px(1.))
                                    .flex_1()
                                    .bg(gpui::rgb(theme::border_subtle())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(gpui::rgb(theme::text_muted()))
                                    .child("or continue with"),
                            )
                            .child(
                                div()
                                    .h(gpui::px(1.))
                                    .flex_1()
                                    .bg(gpui::rgb(theme::border_subtle())),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(oauth_button(OAuthProvider::Github, busy, cx))
                            .child(oauth_button(OAuthProvider::Google, busy, cx))
                            .child(oauth_button(OAuthProvider::Apple, busy, cx)),
                    );
            }
            OAuthFlow::Pending { provider, auth_url } => {
                card = card
                    .child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_primary()))
                            .child(format!(
                                "Finish signing in with {}",
                                provider.label()
                            )),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_secondary()))
                            .child("Your browser opened the sign-in page. After you approve, the site redirects you; paste that final URL here."),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .line_clamp(2)
                            .child(auth_url.clone()),
                    )
                    .child(field("", self.callback_input.clone()))
                    .child(
                        widgets::primary_button("oauth-confirm")
                            .w_full()
                            .when(busy, |el| el.bg(gpui::rgb(theme::bg_sidebar_pill())))
                            .when(!busy, |el| {
                                el.on_click(cx.listener(|this, _event, _window, cx| {
                                    this.confirm_oauth(cx);
                                }))
                            })
                            .child(if busy {
                                "Completing…".to_string()
                            } else {
                                "Complete sign in".to_string()
                            }),
                    )
                    .child(
                        widgets::ghost_button("oauth-cancel")
                            .w_full()
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.cancel_oauth(cx);
                            }))
                            .child("Back to email sign in"),
                    );
            }
        }

        if let Some(message) = self.error.clone() {
            card = card.child(widgets::banner(theme::status_error()).child(message));
        }

        div()
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .justify_center()
            .items_center()
            .bg(gpui::rgb(theme::bg_app()))
            .child(crate::ui::titlebar::drag_strip())
            .child(card)
    }
}

fn oauth_button(
    provider: OAuthProvider,
    busy: bool,
    cx: &mut Context<LoginScreen>,
) -> gpui::Stateful<Div> {
    widgets::secondary_button(gpui::SharedString::from(format!(
        "oauth-{}",
        provider.label().to_lowercase()
    )))
    .flex_1()
    .when(!busy, |el| {
        el.on_click({
            cx.listener(move |this, _event, _window, cx| {
                this.start_oauth(provider, cx);
            })
        })
    })
    .child(provider.label().to_string())
}

fn field(label: &str, input: Entity<TextInput>) -> Div {
    let mut container = div().flex().flex_col().gap_1();
    if !label.is_empty() {
        container = container.child(
            div()
                .text_sm()
                .text_color(gpui::rgb(theme::text_secondary()))
                .child(label.to_string()),
        );
    }
    container.child(widgets::input_frame().child(input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::{cell::RefCell, rc::Rc};

    #[gpui::test]
    fn oauth_committed_success_is_delivered_when_back_precedes_ui_receipt(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let backend =
            Arc::new(AgentBackend::new("http://127.0.0.1:9".to_string(), String::new()).unwrap());
        let screen = cx.new(|cx| LoginScreen::new(backend, cx));
        let signed_in = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&signed_in);
        let _subscription = cx.update(|cx| {
            cx.subscribe(&screen, move |_, event: &LoginSucceeded, _| {
                observed.borrow_mut().push(event.0.clone());
            })
        });

        // The backend has completed publication, but its bridge has not
        // delivered the successful result to the login screen yet.
        let committed = Ok(AuthSession {
            user_id: "oauth-account-fixture".to_string(),
        });
        screen.update(cx, |this, cx| {
            this.oauth = OAuthFlow::Pending {
                provider: OAuthProvider::Github,
                auth_url: "https://example.com/authorize".to_string(),
            };
            this.busy = true;
            this.cancel_oauth(cx);
            assert!(
                this.busy,
                "Back must not admit a replacement sign-in before receipt"
            );
            assert!(matches!(this.oauth, OAuthFlow::Idle));

            // Deliver the same completion handler that the real bridge uses.
            this.busy = false;
            this.oauth_completed(committed, cx);
            assert!(this.error.is_none());
        });
        assert_eq!(&*signed_in.borrow(), &["oauth-account-fixture"]);

        screen.update(cx, |this, cx| {
            this.oauth_completed(Err("Sign in was cancelled. Start again.".to_string()), cx);
            assert!(
                this.error.is_none(),
                "an actual cancellation stays on the clean login form"
            );
        });
        assert_eq!(signed_in.borrow().len(), 1);
    }
}
