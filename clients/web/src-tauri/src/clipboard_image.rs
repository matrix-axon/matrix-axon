//! WebKitGTK can deliver an empty paste event for an image. Read the same
//! display's CLIPBOARD selection through GTK and return the encoded raster.

use gtk::{
    gdk,
    prelude::{CastNone, GtkWindowExt, WidgetExt},
};
use std::time::Duration;
use tauri::{ipc::Response, Runtime, WebviewWindow};
use tokio::sync::{oneshot, Semaphore};

// Match the attachment staging limit. Check GTK's byte count before copying
// the selection into Rust or handing it to the WebView.
const MAX_IMAGE_BYTES: i32 = 100 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const IMAGE_TARGETS: [&str; 7] = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/bmp",
    "image/tiff",
    "image/avif",
];

// GTK owns the callbacks. Keep the permit until GTK finishes even if the IPC
// request times out, so a stalled clipboard owner cannot queue up reads.
static CLIPBOARD_READ: Semaphore = Semaphore::const_new(1);

#[tauri::command]
pub(crate) async fn read_clipboard_image<R: Runtime>(
    window: WebviewWindow<R>,
) -> Result<Response, String> {
    if window.label() != "main" {
        return Err("clipboard_unavailable".into());
    }
    let permit = CLIPBOARD_READ.try_acquire().map_err(|_| "clipboard_busy")?;
    let (reply, received) = oneshot::channel();
    window
        .with_webview(move |view| {
            let widget = view.inner();
            // Check GTK's active-window state at the read, without dispatching
            // another Tauri window command from the GTK callback.
            let focused = widget
                .toplevel()
                .and_downcast::<gtk::Window>()
                .is_some_and(|window| window.is_active());
            if !focused || reply.is_closed() {
                let _ = reply.send(Err("clipboard_unavailable"));
                return;
            }
            let clipboard = widget.clipboard(&gdk::SELECTION_CLIPBOARD);
            clipboard.request_contents(&gdk::Atom::intern("TARGETS"), move |clipboard, data| {
                let target = data.targets().and_then(|offered| {
                    IMAGE_TARGETS
                        .iter()
                        .map(|name| gdk::Atom::intern(name))
                        .find(|target| offered.contains(target))
                });
                if let Some(target) = target.filter(|_| !reply.is_closed()) {
                    clipboard.request_contents(&target, move |_, data| {
                        let _permit = permit;
                        if reply.is_closed() {
                            return;
                        }
                        let length = data.length();
                        let result = if length > MAX_IMAGE_BYTES {
                            Err("clipboard_too_large")
                        } else if length <= 0 || data.format() != 8 {
                            Err("clipboard_unavailable")
                        } else {
                            Ok(data.data())
                        };
                        let _ = reply.send(result);
                    });
                } else {
                    let _ = reply.send(Ok(Vec::new()));
                }
            });
        })
        .map_err(|_| "clipboard_unavailable")?;

    match tokio::time::timeout(READ_TIMEOUT, received).await {
        Ok(Ok(result)) => result.map(Response::new).map_err(String::from),
        Ok(Err(_)) => Err("clipboard_unavailable".into()),
        Err(_) => Err("clipboard_timed_out".into()),
    }
}
