//! Redraws every window after a display wakes or the display layout changes.
//!
//! Since the pinned GPUI moved macOS 14+ frame pacing to a `CAMetalDisplayLink`
//! (Zed PR 65472), a window can stop painting after sleep, lid close, or a
//! dock or monitor change: input and state keep working, but no frames
//! arrive, until a restart (Maple issue 1640, zed-industries/zed#67004).
//! GPUI wakes the link only when a window first needs a redraw, so a link
//! that went silent is never woken again.
//!
//! On those notifications this asks each window's view to redisplay. GPUI's
//! `displayLayer:` then draws one frame synchronously and attaches a new
//! display link. A pass runs at once and another after displays settle. If
//! no frame request arrives afterwards, a warning records it for diagnosis.

// The pinned objc 0.2 macros expand the historical cargo-clippy feature cfg.
#![allow(unexpected_cfgs)]

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_void};
use std::rc::Rc;
use std::time::Duration;

use gpui::{App, Window};
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel, YES};
use objc::{class, msg_send, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Displays can take a moment to settle after waking or reconfiguring;
/// the second pass catches a window that moved or became visible since.
const SETTLE_DELAY: Duration = Duration::from_secs(1);
/// How long a window that GPUI reports visible may go without a frame
/// request after a pass before that is logged.
const FRAME_DEADLINE: Duration = Duration::from_secs(2);

/// Workspace notifications go to NSWorkspace's own center.
const WORKSPACE_NOTIFICATIONS: [&CStr; 2] = [
    c"NSWorkspaceScreensDidWakeNotification",
    c"NSWorkspaceDidWakeNotification",
];
const APPLICATION_NOTIFICATIONS: [&CStr; 1] =
    [c"NSApplicationDidChangeScreenParametersNotification"];

thread_local! {
    static APP: RefCell<Option<gpui::AsyncApp>> = const { RefCell::new(None) };
    /// Bursts of notifications share one settle pass: only the latest
    /// event's delayed pass runs.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
}

unsafe extern "C" {
    // dispatch_get_main_queue is an SDK inline accessor, not a dylib symbol.
    static _dispatch_main_q: u8;
    fn dispatch_async_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

/// Observe display wake and reconfiguration for the life of the app.
pub(super) fn install(cx: &App) -> Result<(), String> {
    let class = observer_class()?;
    unsafe {
        // Never released: it observes for the life of the process.
        let observer: *mut Object = msg_send![class, new];
        let selector = sel!(displaysChanged:);
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        let workspace_center: *mut Object = msg_send![workspace, notificationCenter];
        let application_center: *mut Object =
            msg_send![class!(NSNotificationCenter), defaultCenter];
        for (center, names) in [
            (workspace_center, &WORKSPACE_NOTIFICATIONS[..]),
            (application_center, &APPLICATION_NOTIFICATIONS[..]),
        ] {
            if center.is_null() {
                return Err("A macOS notification center is unavailable".into());
            }
            for name in names {
                let name: *mut Object =
                    msg_send![class!(NSString), stringWithUTF8String: name.as_ptr()];
                let _: () = msg_send![center, addObserver: observer selector: selector name: name object: std::ptr::null_mut::<Object>()];
            }
        }
    }
    APP.with(|app| *app.borrow_mut() = Some(cx.to_async()));
    Ok(())
}

fn observer_class() -> Result<&'static Class, String> {
    const NAME: &str = "MapleDisplayChangeObserver";
    if let Some(class) = Class::get(NAME) {
        return Ok(class);
    }
    let mut decl = ClassDecl::new(NAME, class!(NSObject))
        .ok_or("Could not declare Maple's display change observer")?;
    unsafe {
        decl.add_method(
            sel!(displaysChanged:),
            displays_changed as extern "C" fn(&Object, Sel, *mut Object),
        );
    }
    Ok(decl.register())
}

extern "C" fn displays_changed(_: &Object, _: Sel, _: *mut Object) {
    // AppKit can post these while GPUI holds its App RefCell. Run the pass
    // on a later main-loop turn, outside this callback.
    let _ = std::panic::catch_unwind(|| unsafe {
        dispatch_async_f(
            std::ptr::addr_of!(_dispatch_main_q).cast_mut().cast(),
            std::ptr::null_mut(),
            begin_redraw,
        );
    });
}

extern "C" fn begin_redraw(_: *mut c_void) {
    let _ = std::panic::catch_unwind(|| {
        let Some(cx) = APP.with(|app| app.borrow().clone()) else {
            return;
        };
        let generation = GENERATION.with(|generation| {
            generation.set(generation.get().wrapping_add(1));
            generation.get()
        });
        cx.update(redraw_windows);
        cx.spawn(async move |cx| {
            cx.background_executor().timer(SETTLE_DELAY).await;
            if GENERATION.with(Cell::get) == generation {
                cx.update(redraw_windows);
            }
        })
        .detach();
    });
}

fn redraw_windows(cx: &mut App) {
    let windows = cx.windows();
    log::debug!("display change: redrawing {} windows", windows.len());
    for handle in windows {
        let _ = handle.update(cx, |_, window, cx| redraw(window, cx));
    }
}

fn redraw(window: &mut Window, cx: &mut App) {
    window.refresh();
    redisplay_view(window);

    let delivered = Rc::new(Cell::new(false));
    window.on_next_frame({
        let delivered = delivered.clone();
        move |_, _| delivered.set(true)
    });
    let handle = Window::window_handle(window);
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FRAME_DEADLINE).await;
        if delivered.get() {
            return;
        }
        let _ = handle.update(cx, |_, window, _| {
            // A hidden window rightly gets no frames.
            if window.is_visible() {
                log::warn!(
                    "no frame request within {FRAME_DEADLINE:?} of a display change; \
                     the window may have stopped painting"
                );
            }
        });
    })
    .detach();
}

/// AppKit calls GPUI's `displayLayer:` on its next commit, outside this
/// update: one synchronous frame, then a newly attached display link.
fn redisplay_view(window: &Window) {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    unsafe {
        let view = handle.ns_view.as_ptr().cast::<Object>();
        let _: () = msg_send![view, setNeedsDisplay: YES];
    }
}
