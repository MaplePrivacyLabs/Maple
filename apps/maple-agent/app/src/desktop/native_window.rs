//! Moves the main window to its saved frame. The pinned GPUI positions a
//! window only when it opens one, and the saved frame arrives later, once
//! the store opens.

// The pinned objc 0.2 macros expand the historical cargo-clippy feature cfg.
#![allow(unexpected_cfgs)]

use gpui::{App, Window};
use objc::runtime::{Object, YES};
use objc::{class, msg_send, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

#[repr(C)]
#[derive(Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

/// Apply the saved size and position in one frame change. Returns false,
/// leaving the window alone, when there is no position or its display is
/// gone.
pub(super) fn place(window: &Window, cx: &App, saved: &crate::settings::WindowState) -> bool {
    let Some(position) = saved.position.as_ref() else {
        return false;
    };
    let Some(display) = super::saved_display(position, cx) else {
        return false;
    };
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return false;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return false;
    };
    unsafe {
        let view = handle.ns_view.as_ptr().cast::<Object>();
        let ns_window: *mut Object = msg_send![view, window];
        if ns_window.is_null() {
            return false;
        }
        let Some(screen) = screen_for(u64::from(display.id())) else {
            return false;
        };
        // GPUI reports the origin from the display's top-left corner;
        // AppKit frames start at the bottom-left of the primary display.
        let screen_frame: NSRect = msg_send![screen, frame];
        let height = f64::from(saved.height);
        let frame = NSRect {
            origin: NSPoint {
                x: screen_frame.origin.x + f64::from(position.x),
                y: screen_frame.origin.y + screen_frame.size.height
                    - f64::from(position.y)
                    - height,
            },
            size: NSSize {
                width: f64::from(saved.width),
                height,
            },
        };
        // Keeps the title bar reachable if the display shrank since.
        let frame: NSRect = msg_send![ns_window, constrainFrameRect: frame toScreen: screen];
        // AppKit reports the move and resize to GPUI synchronously, and the
        // window is borrowed by the caller's update, so set it afterwards,
        // as GPUI's own resize does.
        let ns_window: *mut Object = msg_send![ns_window, retain];
        let ns_window = ns_window as usize;
        cx.foreground_executor()
            .spawn(async move {
                let ns_window = ns_window as *mut Object;
                let () = msg_send![ns_window, setFrame: frame display: YES];
                let () = msg_send![ns_window, release];
            })
            .detach();
    }
    true
}

/// The NSScreen for a CGDirectDisplayID, which is what GPUI's display id
/// holds on macOS.
unsafe fn screen_for(display_id: u64) -> Option<*mut Object> {
    unsafe {
        let key: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: c"NSScreenNumber".as_ptr()];
        let screens: *mut Object = msg_send![class!(NSScreen), screens];
        let count: usize = msg_send![screens, count];
        for index in 0..count {
            let screen: *mut Object = msg_send![screens, objectAtIndex: index];
            let description: *mut Object = msg_send![screen, deviceDescription];
            let number: *mut Object = msg_send![description, objectForKey: key];
            if number.is_null() {
                continue;
            }
            let id: u32 = msg_send![number, unsignedIntValue];
            if u64::from(id) == display_id {
                return Some(screen);
            }
        }
        None
    }
}
