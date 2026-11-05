use objc2::{msg_send, runtime::AnyObject};
use tauri::{webview::PageLoadEvent, webview::PageLoadPayload, Runtime, Webview};

/// Lets WebKit paint the safe-area strips with the page background again.
///
/// wry 0.57 (tauri-apps/wry#1824) makes the iOS WKWebView non-opaque with a
/// `systemBackgroundColor` background. The page uses `viewport-fit=auto`, so
/// the strips beside the status bar and home indicator then show plain white
/// or black instead of the page background. An opaque WebView paints them
/// from the page, as it did before the Tauri 2.12 upgrade.
///
/// This waits for the first page load: an opaque WebView flashes white before
/// the page paints, which is what wry's change avoids at launch.
pub fn on_page_load<R: Runtime>(webview: &Webview<R>, payload: &PageLoadPayload<'_>) {
    if payload.event() != PageLoadEvent::Finished {
        return;
    }

    let result = webview.with_webview(|webview| {
        let wk_webview = webview.inner().cast::<AnyObject>();
        if wk_webview.is_null() {
            return;
        }
        // SAFETY: on iOS `inner()` is the live WKWebView, and `with_webview`
        // runs this closure on the main thread.
        unsafe {
            let _: () = msg_send![&*wk_webview, setOpaque: true];
        }
    });
    if let Err(error) = result {
        log::warn!("Failed to make webview {} opaque: {error}", webview.label());
    }
}
