//! AppKit's termination deferral. GPUI's 200 ms shutdown callback runs after
//! AppKit has already decided to terminate, so it cannot own a host drain.

// The pinned objc 0.2 macros expand the historical cargo-clippy feature cfg.
// Limit this allowance to this native bridge rather than the application.
#![allow(unexpected_cfgs)]

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_void};

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
    fn class_getImageName(class: *const Class) -> *const c_char;
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
    if delegate.is_null() {
        return Err("Maple's native application delegate is missing".into());
    }
    let active = unsafe { (*delegate).class() };
    if active != class {
        return Err(delegate_mismatch(active, class));
    }
    let selector = Sel::register("applicationShouldTerminate:");
    if class.instance_method(selector).is_some() {
        let provider =
            method_provider(class, selector).unwrap_or_else(|| "provider not found".into());
        return Err(format!(
            "The native application delegate already handles termination ({provider}); Maple must review this GPUI integration"
        ));
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

// Startup failures in this bridge happen on users' Macs, where the only
// evidence is the message. Name the classes and libraries involved.

/// Why the active delegate is not the reviewed GPUI delegate. Key-value
/// observing replaces an object's class with a generated subclass, which
/// this names apart from an unrelated delegate.
fn delegate_mismatch(active: &Class, expected: &Class) -> String {
    let relation = if inherits_from(active, expected) {
        "a subclass of"
    } else {
        "not derived from"
    };
    format!(
        "Maple's active native application delegate is {}, {relation} the reviewed {}",
        describe_class(active),
        expected.name()
    )
}

fn inherits_from(class: &Class, ancestor: &Class) -> bool {
    std::iter::successors(Some(class), |class| class.superclass()).any(|class| class == ancestor)
}

/// The class in `class`'s superclass chain that defines `selector` itself,
/// with the library its implementation was loaded from. A category added
/// to `NSResponder` or `NSObject` is found here although GPUI defines none.
fn method_provider(class: &Class, selector: Sel) -> Option<String> {
    // Walk up while the superclass resolves the same method. objc 0.2's
    // `instance_methods` builds a misaligned slice for a class with no
    // methods of its own, so the method lists are not read directly.
    let method = class.instance_method(selector)?;
    let mut owner = class;
    while let Some(superclass) = owner.superclass() {
        if !superclass
            .instance_method(selector)
            .is_some_and(|inherited| std::ptr::eq(inherited, method))
        {
            break;
        }
        owner = superclass;
    }
    let address = method.implementation() as *const c_void;
    Some(match image_containing(address) {
        Some(image) => format!("from {} in {image}", owner.name()),
        None => format!("from {}", owner.name()),
    })
}

fn describe_class(class: &Class) -> String {
    // SAFETY: `class` is a registered class. The runtime returns a static
    // path or null for a class created at run time (key-value observing).
    let image = unsafe { class_getImageName(class) };
    match c_path(image) {
        Some(image) => format!("{} in {image}", class.name()),
        None => class.name().to_string(),
    }
}

fn image_containing(address: *const c_void) -> Option<String> {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: dladdr only reads `address` and fills `info`; its name stays
    // valid while the image is loaded, and it is copied at once.
    if unsafe { libc::dladdr(address, &mut info) } == 0 {
        return None;
    }
    c_path(info.dli_fname)
}

fn c_path(path: *const c_char) -> Option<String> {
    if path.is_null() {
        return None;
    }
    // SAFETY: non-null NUL-terminated strings owned by the runtime or dyld.
    let path = unsafe { CStr::from_ptr(path) }.to_string_lossy();
    (!path.is_empty()).then(|| path.into_owned())
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

#[cfg(test)]
mod tests {
    use super::*;
    use objc::declare::ClassDecl;

    extern "C" fn terminate_later(_: &Object, _: Sel, _: *mut Object) -> usize {
        2
    }

    extern "C" fn level(_: &Object, _: Sel) -> *mut Object {
        std::ptr::null_mut()
    }

    extern "C" fn set_level(_: &Object, _: Sel, _: *mut Object) {}

    fn declare(name: &str, superclass: &Class) -> ClassDecl {
        ClassDecl::new(name, superclass).expect("each test class name is unique")
    }

    #[test]
    fn method_provider_names_the_defining_ancestor_and_its_library() {
        let selector = sel!(applicationShouldTerminate:);
        let plain = declare("MapleQuitTestPlainDelegate", class!(NSResponder)).register();
        assert!(plain.instance_method(selector).is_none());
        assert_eq!(method_provider(plain, selector), None);

        let mut base = declare("MapleQuitTestTerminatingBase", class!(NSResponder));
        unsafe {
            base.add_method(
                selector,
                terminate_later as extern "C" fn(&Object, Sel, *mut Object) -> usize,
            );
        }
        let base = base.register();
        let child = declare("MapleQuitTestTerminatingChild", base).register();
        // The install guard's inherited lookup sees the ancestor's method.
        assert!(child.instance_method(selector).is_some());
        let provider = method_provider(child, selector).unwrap();
        assert!(
            provider.starts_with("from MapleQuitTestTerminatingBase in /"),
            "{provider}"
        );
    }

    #[test]
    fn delegate_mismatch_tells_an_observed_subclass_from_another_class() {
        let mut decl = declare("MapleQuitTestObservedDelegate", class!(NSObject));
        unsafe {
            decl.add_method(
                sel!(level),
                level as extern "C" fn(&Object, Sel) -> *mut Object,
            );
            decl.add_method(
                sel!(setLevel:),
                set_level as extern "C" fn(&Object, Sel, *mut Object),
            );
        }
        let reviewed = decl.register();
        unsafe {
            let pool: *mut Object = msg_send![class!(NSAutoreleasePool), new];
            let delegate: *mut Object = msg_send![reviewed, new];
            let observer: *mut Object = msg_send![class!(NSObject), new];
            let key: *mut Object =
                msg_send![class!(NSString), stringWithUTF8String: c"level".as_ptr()];
            let _: () = msg_send![delegate, addObserver: observer forKeyPath: key options: 0usize context: std::ptr::null_mut::<c_void>()];
            let observed = (*delegate).class();
            assert!(observed != reviewed);
            let message = delegate_mismatch(observed, reviewed);
            assert_eq!(
                message,
                "Maple's active native application delegate is \
                 NSKVONotifying_MapleQuitTestObservedDelegate, a subclass of the reviewed \
                 MapleQuitTestObservedDelegate"
            );
            let _: () = msg_send![delegate, removeObserver: observer forKeyPath: key];
            let _: () = msg_send![delegate, release];
            let _: () = msg_send![observer, release];
            let _: () = msg_send![pool, drain];
        }
        let unrelated = delegate_mismatch(class!(NSObject), reviewed);
        assert!(
            unrelated.starts_with("Maple's active native application delegate is NSObject in /"),
            "{unrelated}"
        );
        assert!(
            unrelated.ends_with(", not derived from the reviewed MapleQuitTestObservedDelegate"),
            "{unrelated}"
        );
    }
}
