//! macOS Apple Passwords sheet for the trymaple.ai credential.
//!
//! The desktop webview origin is `tauri://localhost`, so HTML autofill cannot
//! see a password saved for trymaple.ai. `ASAuthorizationPasswordProvider`
//! asks the system instead. The associated-domain entitlement, not this
//! command, decides which site's passwords are eligible. The controller keeps
//! its delegate weakly, so the in-flight request retains the delegate until
//! the sheet finishes.
//!
//! The command is async: AppKit delivers the delegate callbacks on the main
//! thread, so the caller must wait off the main thread.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message,
};
use objc2_app_kit::NSWindow;
use objc2_authentication_services::{
    ASAuthorization, ASAuthorizationController, ASAuthorizationControllerDelegate,
    ASAuthorizationControllerPresentationContextProviding, ASAuthorizationPasswordProvider,
    ASAuthorizationRequest, ASPasswordCredential, ASPresentationAnchor,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use serde::Serialize;
use tauri::{AppHandle, Manager};
use tokio::sync::oneshot;

use crate::apple_password_decision::{
    accept_apple_password, apple_password_failure_message, classify_apple_authorization_code,
    ApplePasswordDecision,
};

const SHEET_UNAVAILABLE: &str = "Apple Passwords could not be opened.";
const SHEET_BUSY: &str = "Apple Passwords is already open.";

type DecisionSender = oneshot::Sender<ApplePasswordDecision>;

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
    sender: Cell<Option<DecisionSender>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the delegate does
    // not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PasswordDelegateIvars]
    struct MapleApplePasswordDelegate;

    unsafe impl NSObjectProtocol for MapleApplePasswordDelegate {}

    unsafe impl ASAuthorizationControllerDelegate for MapleApplePasswordDelegate {
        #[unsafe(method(authorizationController:didCompleteWithAuthorization:))]
        fn did_complete_with_authorization(
            &self,
            _controller: &ASAuthorizationController,
            authorization: &ASAuthorization,
        ) {
            self.finish(decision_from_authorization(authorization));
        }

        #[unsafe(method(authorizationController:didCompleteWithError:))]
        fn did_complete_with_error(
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
        #[unsafe(method_id(presentationAnchorForAuthorizationController:))]
        fn presentation_anchor(
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

impl MapleApplePasswordDelegate {
    fn new(
        mtm: MainThreadMarker,
        window: Retained<NSWindow>,
        sender: DecisionSender,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PasswordDelegateIvars {
            window,
            sender: Cell::new(Some(sender)),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn finish(&self, decision: ApplePasswordDecision) {
        if let Some(sender) = self.ivars().sender.take() {
            let _ = sender.send(decision);
        }
        // The controller does not retain its delegate. Dropping the in-flight
        // request here is safe only while this extra retain keeps the object
        // alive until the method returns.
        let _keep_alive = self.retain();
        let request = IN_FLIGHT.with(|slot| slot.borrow_mut().take());
        drop(request);
    }
}

struct InFlightRequest {
    _delegate: Retained<MapleApplePasswordDelegate>,
    _controller: Retained<ASAuthorizationController>,
}

thread_local! {
    static IN_FLIGHT: RefCell<Option<InFlightRequest>> = const { RefCell::new(None) };
}

fn decision_from_authorization(authorization: &ASAuthorization) -> ApplePasswordDecision {
    let credential = unsafe { authorization.credential() };
    let object: &AnyObject = credential.as_ref();
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

fn main_ns_window(app: &AppHandle) -> Result<(MainThreadMarker, Retained<NSWindow>), &'static str> {
    let mtm = MainThreadMarker::new().ok_or(SHEET_UNAVAILABLE)?;
    if IN_FLIGHT.with(|slot| slot.borrow().is_some()) {
        return Err(SHEET_BUSY);
    }
    let window = app.get_webview_window("main").ok_or(SHEET_UNAVAILABLE)?;
    let ns_window = window.ns_window().map_err(|_| {
        log::info!("Apple Passwords could not read the main window");
        SHEET_UNAVAILABLE
    })?;
    // SAFETY: Tauri returns the live NSWindow for this WebviewWindow, and this
    // runs on the macOS main thread. The pointer is retained before the raw
    // reference is dropped.
    let ns_window = unsafe { ns_window.cast::<NSWindow>().as_ref() }.ok_or(SHEET_UNAVAILABLE)?;
    Ok((mtm, ns_window.retain()))
}

fn present_apple_password(app: &AppHandle, sender: DecisionSender) {
    let (mtm, window) = match main_ns_window(app) {
        Ok(found) => found,
        Err(message) => {
            let _ = sender.send(ApplePasswordDecision::Rejected { message });
            return;
        }
    };
    let delegate = MapleApplePasswordDelegate::new(mtm, window, sender);
    let provider = unsafe { ASAuthorizationPasswordProvider::new() };
    let request = unsafe { provider.createRequest() };
    let request: Retained<ASAuthorizationRequest> = Retained::into_super(request);
    let requests = NSArray::from_retained_slice(&[request]);
    let controller = unsafe {
        ASAuthorizationController::initWithAuthorizationRequests(
            ASAuthorizationController::alloc(),
            &requests,
        )
    };
    unsafe {
        controller.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        controller.setPresentationContextProvider(Some(ProtocolObject::from_ref(&*delegate)));
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
pub async fn request_apple_password(app: AppHandle) -> Result<ApplePasswordResponse, String> {
    let (sender, receiver) = oneshot::channel();
    let app_for_sheet = app.clone();
    app.run_on_main_thread(move || present_apple_password(&app_for_sheet, sender))
        .map_err(|_| SHEET_UNAVAILABLE.to_string())?;
    receiver
        .await
        .map(response_from_decision)
        .map_err(|_| "Apple Passwords closed before a credential was chosen.".to_string())
}
