//! macOS Apple Passwords sheet for the trymaple.ai credential.
//!
//! The desktop webview origin is `tauri://localhost`, so HTML autofill cannot
//! see a password saved for trymaple.ai. `ASAuthorizationPasswordProvider`
//! asks the system instead. The associated-domain entitlement, not this
//! command, decides which site's passwords are eligible. The controller keeps
//! its delegate weakly, so the in-flight request retains the delegate until
//! the sheet finishes.

use std::cell::RefCell;
use std::sync::mpsc::{self, Sender};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::NSWindow;
use objc2_authentication_services::{
    ASAuthorization, ASAuthorizationController, ASAuthorizationControllerDelegate,
    ASAuthorizationControllerPresentationContextProviding, ASAuthorizationPasswordProvider,
    ASAuthorizationRequest, ASPasswordCredential, ASPresentationAnchor,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::apple_password_decision::{
    accept_apple_password, apple_password_failure_message, classify_apple_authorization_code,
    ApplePasswordDecision,
};

const SHEET_UNAVAILABLE: &str = "Apple Passwords could not be opened.";
const SHEET_BUSY: &str = "Apple Passwords is already open.";

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ApplePasswordResponse {
    Selected { username: String, password: String },
    Cancelled,
    Unavailable,
    Failed { message: String },
}

struct PasswordDelegateIvars {
    window: Retained<NSWindow>,
    sender: std::sync::Mutex<Option<Sender<ApplePasswordDecision>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PasswordDelegateIvars]
    struct MapleApplePasswordDelegate;

    impl MapleApplePasswordDelegate {
        fn new(
            mtm: objc2::MainThreadMarker,
            window: Retained<NSWindow>,
            sender: Sender<ApplePasswordDecision>,
        ) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(PasswordDelegateIvars {
                window,
                sender: std::sync::Mutex::new(Some(sender)),
            });
            unsafe { msg_send![super(this), init] }
        }

        fn finish(&self, decision: ApplePasswordDecision) {
            // The controller does not retain its delegate. Dropping the
            // in-flight request from inside this callback is safe only while
            // this extra retain keeps the object alive until the method returns.
            let _keep_alive = self.retain();
            if let Some(sender) = self
                .ivars()
                .sender
                .lock()
                .ok()
                .and_then(|mut sender| sender.take())
            {
                let _ = sender.send(decision);
            }
            IN_FLIGHT.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    unsafe impl NSObjectProtocol for MapleApplePasswordDelegate {}

    unsafe impl ASAuthorizationControllerDelegate for MapleApplePasswordDelegate {
        #[unsafe(method(authorizationController:didCompleteWithAuthorization:))]
        fn authorizationController_didCompleteWithAuthorization(
            &self,
            _controller: &ASAuthorizationController,
            authorization: &ASAuthorization,
        ) {
            self.finish(decision_from_authorization(authorization));
        }

        #[unsafe(method(authorizationController:didCompleteWithError:))]
        fn authorizationController_didCompleteWithError(
            &self,
            _controller: &ASAuthorizationController,
            error: &NSError,
        ) {
            let code = error.code() as i64;
            let decision = classify_apple_authorization_code(code);
            if matches!(decision, ApplePasswordDecision::Failed { .. }) {
                log::info!("Apple Passwords request failed with code {code}");
            }
            self.finish(decision);
        }
    }

    unsafe impl ASAuthorizationControllerPresentationContextProviding for MapleApplePasswordDelegate {
        #[unsafe(method(presentationAnchorForAuthorizationController:))]
        fn presentationAnchorForAuthorizationController(
            &self,
            _controller: &ASAuthorizationController,
        ) -> Retained<ASPresentationAnchor> {
            // The generated binding types the anchor as NSObject. The object
            // is still the NSWindow AppKit presents the sheet from.
            let window = self.ivars().window.clone();
            let responder = Retained::into_super(window);
            Retained::into_super(responder)
        }
    }
);

struct InFlightRequest {
    _delegate: Retained<MapleApplePasswordDelegate>,
    _controller: Retained<ASAuthorizationController>,
}

thread_local! {
    static IN_FLIGHT: RefCell<Option<InFlightRequest>> = const { RefCell::new(None) };
}

fn decision_from_authorization(authorization: &ASAuthorization) -> ApplePasswordDecision {
    let credential = unsafe { authorization.credential() };
    let object: &AnyObject = AsRef::<AnyObject>::as_ref(&*credential);
    let Some(password_credential) = object.downcast_ref::<ASPasswordCredential>() else {
        return ApplePasswordDecision::Unavailable;
    };
    let username = unsafe { password_credential.user() };
    let password = unsafe { password_credential.password() };
    accept_apple_password(&username.to_string(), &password.to_string())
}

fn response_from_decision(decision: ApplePasswordDecision) -> ApplePasswordResponse {
    match decision {
        ApplePasswordDecision::Selected { username, password } => {
            ApplePasswordResponse::Selected { username, password }
        }
        ApplePasswordDecision::Cancelled => ApplePasswordResponse::Cancelled,
        ApplePasswordDecision::Unavailable => ApplePasswordResponse::Unavailable,
        ApplePasswordDecision::Failed { .. } => ApplePasswordResponse::Failed {
            message: apple_password_failure_message(&ApplePasswordDecision::Failed { code: 0 })
                .unwrap_or("Apple Passwords could not be used. Try again.")
                .to_string(),
        },
        ApplePasswordDecision::Rejected { message } => ApplePasswordResponse::Failed {
            message: message.to_string(),
        },
    }
}

fn send_decision(sender: &Sender<ApplePasswordDecision>, decision: ApplePasswordDecision) {
    let _ = sender.send(decision);
}

fn present_apple_password(app: &AppHandle, sender: Sender<ApplePasswordDecision>) {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        send_decision(
            &sender,
            ApplePasswordDecision::Rejected {
                message: SHEET_UNAVAILABLE,
            },
        );
        return;
    };

    if IN_FLIGHT.with(|slot| slot.borrow().is_some()) {
        send_decision(
            &sender,
            ApplePasswordDecision::Rejected {
                message: SHEET_BUSY,
            },
        );
        return;
    }

    let Some(window) = app.get_webview_window("main") else {
        send_decision(
            &sender,
            ApplePasswordDecision::Rejected {
                message: SHEET_UNAVAILABLE,
            },
        );
        return;
    };
    let ns_window = match window.ns_window() {
        Ok(ns_window) => ns_window.cast::<NSWindow>(),
        Err(_) => {
            log::info!("Apple Passwords could not read the main window");
            send_decision(
                &sender,
                ApplePasswordDecision::Rejected {
                    message: SHEET_UNAVAILABLE,
                },
            );
            return;
        }
    };
    // SAFETY: Tauri returns the live NSWindow for this WebviewWindow, and this
    // closure runs on the macOS main thread. The pointer is retained before
    // the raw reference is dropped.
    let Some(ns_window) = (unsafe { ns_window.as_ref() }) else {
        send_decision(
            &sender,
            ApplePasswordDecision::Rejected {
                message: SHEET_UNAVAILABLE,
            },
        );
        return;
    };
    let window = ns_window.retain();
    let delegate = MapleApplePasswordDelegate::new(mtm, window, sender);
    let provider = unsafe { ASAuthorizationPasswordProvider::new() };
    let request = unsafe { provider.createRequest() };
    let request: Retained<ASAuthorizationRequest> = Retained::into_super(request);
    let requests = NSArray::from_slice(&[request.as_ref()]);
    let controller = unsafe {
        ASAuthorizationController::initWithAuthorizationRequests(
            ASAuthorizationController::alloc(),
            &requests,
        )
    };
    let delegate_ref: &MapleApplePasswordDelegate = &delegate;
    unsafe {
        controller.setDelegate(Some(ProtocolObject::from_ref(delegate_ref)));
        controller.setPresentationContextProvider(Some(ProtocolObject::from_ref(delegate_ref)));
    }
    let controller_for_request = controller.clone();
    IN_FLIGHT.with(|slot| {
        *slot.borrow_mut() = Some(InFlightRequest {
            _delegate: delegate,
            _controller: controller,
        });
    });
    unsafe { controller_for_request.performRequests() };
}

#[tauri::command]
pub fn request_apple_password(app: AppHandle) -> Result<ApplePasswordResponse, String> {
    let (sender, receiver) = mpsc::channel();
    let app_for_sheet = app.clone();
    app.run_on_main_thread(move || present_apple_password(&app_for_sheet, sender))
        .map_err(|_| SHEET_UNAVAILABLE.to_string())?;
    match receiver.recv() {
        Ok(decision) => Ok(response_from_decision(decision)),
        Err(_) => Err("Apple Passwords closed before a credential was chosen.".to_string()),
    }
}
