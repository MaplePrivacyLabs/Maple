//! AppKit's termination deferral. GPUI's 200 ms shutdown callback runs after
//! AppKit has already decided to terminate, so it cannot own a host drain.

// The pinned objc 0.2 macros expand the historical cargo-clippy feature cfg.
// Limit this allowance to this native bridge rather than the application.
#![allow(unexpected_cfgs)]

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use objc::runtime::{Class, Imp, NO, Object, Sel, YES};
use objc::{class, msg_send, sel, sel_impl};

thread_local! {
    static APP: RefCell<Option<gpui::AsyncApp>> = const { RefCell::new(None) };
    static PENDING: Cell<bool> = const { Cell::new(false) };
    static TERMINATION_ALLOWED: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "C" {
    // dispatch_get_main_queue is an SDK inline accessor, not a dylib symbol.
    static _dispatch_main_q: u8;
    fn dispatch_async_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn pthread_main_np() -> std::ffi::c_int;
}

fn main_queue() -> *mut c_void {
    std::ptr::addr_of!(_dispatch_main_q).cast_mut().cast()
}

/// Add only the optional delegate method missing from the pinned GPUI. Do
/// not replace a future GPUI implementation: that integration needs review.
pub(super) fn install(cx: &gpui::App) -> Result<(), String> {
    if unsafe { pthread_main_np() } != 1 {
        return Err("Maple's termination coordinator requires the main thread".into());
    }
    let class = Class::get("GPUIApplicationDelegate")
        .ok_or("Maple's native application delegate is unavailable")?;
    let application: *mut Object = unsafe { msg_send![class!(NSApplication), sharedApplication] };
    let delegate: *mut Object = unsafe { msg_send![application, delegate] };
    if delegate.is_null() || unsafe { (*delegate).class() } != class {
        return Err(
            "Maple's active native application delegate differs from the reviewed GPUI delegate"
                .into(),
        );
    }
    let selector = Sel::register("applicationShouldTerminate:");
    if class.instance_method(selector).is_some() {
        return Err("The native application delegate already handles termination; Maple must review this GPUI integration".into());
    }
    // NSApplicationTerminateReply is NSUInteger (Q on supported 64-bit Macs).
    let implementation: Imp = unsafe {
        std::mem::transmute::<extern "C" fn(&Object, Sel, *mut Object) -> usize, Imp>(
            should_terminate,
        )
    };
    let added = unsafe {
        objc::runtime::class_addMethod(
            std::ptr::from_ref(class).cast_mut(),
            selector,
            implementation,
            c"Q@:@".as_ptr(),
        )
    };
    if added != YES {
        return Err("Could not install Maple's native termination coordinator".into());
    }
    APP.with(|app| *app.borrow_mut() = Some(cx.to_async()));
    Ok(())
}

extern "C" fn should_terminate(_: &Object, _: Sel, _: *mut Object) -> usize {
    std::panic::catch_unwind(|| {
        if TERMINATION_ALLOWED.with(Cell::get) {
            // NSTerminateNow: either the host stopped or the frontend committed
            // an intentional detach. Only the former proves owner-lock release.
            return 1;
        }
        if APP.with(|app| app.borrow().is_none()) {
            return 0; // NSTerminateCancel; there is no safe drain coordinator.
        }
        if !PENDING.with(|pending| pending.replace(true)) {
            // AppKit can call us while GPUI holds its App RefCell. Queue the
            // update for a later main-loop turn, outside this FFI callback.
            unsafe {
                dispatch_async_f(main_queue(), std::ptr::null_mut(), begin_quit);
            }
        }
        2 // NSTerminateLater, paired with exactly one asynchronous reply.
    })
    .unwrap_or(0)
}

extern "C" fn begin_quit(_: *mut c_void) {
    if std::panic::catch_unwind(|| {
        APP.with(|app| {
            if let Some(cx) = app.borrow().clone() {
                cx.update(super::quit_app);
            }
        });
    })
    .is_err()
    {
        cancel();
    }
}

/// The reply itself can synchronously invoke GPUI's shutdown callback. Keep
/// it outside an App update, just as GPUI defers NSApplication::terminate.
pub(super) fn complete(cx: &mut gpui::App) {
    allow_termination(cx);
}

/// Called only after an update commit retained owner.lock for process exit.
/// Allow GPUI's restart termination, without quitting before its helper starts.
pub(super) fn allow_update_restart() {
    TERMINATION_ALLOWED.with(|allowed| allowed.set(true));
}

/// GPUI's helper has now been requested. Complete an already deferred native
/// Quit, while leaving the retry UI alive if no Quit was pending and spawn fails.
pub(super) fn finish_update_restart() {
    if PENDING.with(Cell::get) {
        defer_reply(true);
    }
}

pub(super) fn is_pending() -> bool {
    PENDING.with(Cell::get)
}

/// A saved, intentional frontend detach permits app termination without
/// claiming that the independently owned host stopped or its data was locked.
pub(super) fn complete_frontend_detach(cx: &mut gpui::App) {
    allow_termination(cx);
}

#[cfg(test)]
pub(super) fn reset_frontend_detach_for_test() {
    assert!(!PENDING.with(Cell::get));
    TERMINATION_ALLOWED.with(|allowed| allowed.set(false));
}

fn allow_termination(cx: &mut gpui::App) {
    TERMINATION_ALLOWED.with(|allowed| allowed.set(true));
    if PENDING.with(Cell::get) {
        defer_reply(true);
    } else {
        cx.quit();
    }
}

pub(super) fn cancel() {
    if PENDING.with(Cell::get) {
        defer_reply(false);
    }
}

fn defer_reply(allow: bool) {
    unsafe {
        dispatch_async_f(main_queue(), usize::from(allow) as *mut c_void, reply);
    }
}

extern "C" fn reply(context: *mut c_void) {
    let _ = std::panic::catch_unwind(|| {
        if !PENDING.with(|pending| pending.replace(false)) {
            return;
        }
        let allow = !context.is_null();
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let _: () =
                msg_send![app, replyToApplicationShouldTerminate: if allow { YES } else { NO }];
        }
    });
}
