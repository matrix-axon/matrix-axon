//! The Axon native shell (ADR 0102, M-W12).
//!
//! This process owns two things the webview cannot do for itself.
//!
//! **Transport.** The page loads from a custom scheme, so every `/v1` call is
//! cross-origin, and the server serves no CORS headers (ADR 0046's M-W1.5 was
//! designed and never built; ADR 0052 § 5 chose a same-origin front door
//! instead). `tauri-plugin-http` and `tauri-plugin-websocket` move those calls
//! into this process, where CORS does not apply and a plain-http LAN server is
//! reachable — see ADR 0102 § 2.
//!
//! **Serving the bundle**, so an unknown path can fall back to the app instead
//! of 404ing. See `route`.
//!
//! **Two affordances the webview has no working default for.** Saving a file:
//! `<a download>` is inert from a custom scheme, so the app asks the OS for a
//! path and writes the bytes itself. Opening an external link: left alone it
//! navigates the *app window* to that page, and there is no back button to
//! return with, so links are handed to the user's real browser.
//!
//! **Reading a dropped file, on Linux.** WebKitGTK hands the page a
//! `text/uri-list` and no `File`, so HTML5 drag-and-drop there gives the app a
//! path it cannot open. This process takes the drag at the window instead and
//! reads the bytes — see `read_dropped_file`.

/// Wire up and run the shell.
///
/// `pub` and in the library rather than `main.rs` because the mobile targets
/// (M-W13) link this crate and call in through their own generated entry
/// point; the desktop binary is a one-line caller of the same function.
pub fn run() {
    // Before anything touches the webview: WebKitGTK has to be told not to use
    // its DMA-BUF renderer, or it draws nothing where a `<canvas>` should be.
    #[cfg(target_os = "linux")]
    keep_canvas_content();

    let builder = tauri::Builder::default();

    // Must be the *first* plugin registered, per its own documentation: it
    // decides whether this process lives at all, and anything set up before it
    // would be initialised in a process about to exit.
    //
    // Windows and Linux answer a deep link by spawning a new process with the
    // URL as a CLI argument, rather than signalling the running app. Left
    // alone, the OAuth callback would arrive in a second instance — one whose
    // webview has never seen the PKCE verifier, which lives in the *first*
    // instance's sessionStorage — and the exchange would fail with the sign-in
    // apparently having worked. Its `deep-link` feature hands the URL to the
    // running instance instead, which is where the flow started.
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        // The URL has already been dispatched to the deep-link plugin by
        // the time this runs. All that is left is to raise the window the
        // user was last looking at, since they are coming back from a
        // browser and expect to land in the app.
        use tauri::Manager as _;
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_focus();
        }
    }));

    builder
        .manage(DroppedPaths::default())
        .invoke_handler(tauri::generate_handler![read_dropped_file])
        // Transport. Both are configured by capability files under
        // `capabilities/`, not here — the allow-list of reachable origins is
        // security-relevant and belongs somewhere reviewable.
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_websocket::init())
        // Saving an attachment: a save dialog and a real write, because
        // `<a download>` does nothing from a custom scheme.
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        // External links, opened in the user's browser rather than in place.
        .plugin(tauri_plugin_opener::init())
        // The OAuth callback. Sign-in happens in the user's real browser
        // (RFC 8252), which redirects to this app's registered scheme.
        .plugin(tauri_plugin_deep_link::init())
        // Serve the bundle ourselves, so an unknown path can fall back to the
        // app instead of 404ing. See `route`.
        .register_uri_scheme_protocol(APP_SCHEME, |ctx, request| {
            serve(ctx.app_handle(), request.uri().path())
        })
        .setup(|app| {
            let window = main_window(app.handle())?;
            allow_camera_capture(&window);
            watch_dropped_paths(&window);
            claim_deep_link_schemes(app.handle());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Axon shell");
}

/// Claim the OAuth deep-link scheme with the OS — in development builds only.
///
/// The scheme is `org.matrixaxon.axon`, named once in
/// `tauri.conf.json`'s `plugins.deep-link.desktop.schemes` and read from there;
/// nothing here hard-codes it. Reverse-domain per RFC 8252 § 7.1, because a
/// private-use scheme is claimed first-come on every desktop OS and a short one
/// is trivial to collide with or impersonate.
///
/// Not to be confused with [`APP_SCHEME`], which is also spelled `axon` and is
/// a different thing entirely: an in-webview protocol handler that serves the
/// bundle, never registered with the OS, taking no part in OAuth.
/// Answer WebKitGTK's camera permission request, on Linux only.
///
/// Every desktop webview gates `getUserMedia`, and they disagree on the
/// default. WebView2 *prompts the user*, which is why Windows works with no
/// code. WKWebView asks its UI delegate, and wry answers `Grant`, so macOS
/// needs nothing here either. WebKitGTK emits `permission-request` and
/// **denies** when nothing handles the signal — and wry's GTK backend handles
/// no permissions at all, so the request was refused before any prompt could
/// exist. What the user saw was the browser's own wording for a denial they
/// were never asked about, after granting camera access at the OS level and
/// even adding themselves to the `video` group, neither of which WebKit
/// consults.
///
/// Granting without prompting is the right answer *here* specifically. The
/// webview loads one thing — this app's own bundle, from its own scheme — so
/// there is no third-party page to protect the camera from. And the request
/// only ever follows the user pressing "Start camera", which is the consent; a
/// second dialog asking whether they meant it would be noise.
///
/// That premise is enforced by `main_window`'s navigation guard, not by the
/// CSP. `default-src 'self'` says where resources may be *fetched* from and
/// says nothing about where the top-level document may *navigate* — so a CSP
/// alone would leave this grant resting on the client never following an
/// off-origin link, which is a property of today's code rather than of the
/// window. See `main_window`.
///
/// Narrow twice over. Only user-media requests are answered, so geolocation,
/// notifications and the rest keep WebKit's deny-by-default; and within those,
/// only video.
#[cfg(target_os = "linux")]
fn allow_camera_capture<R: tauri::Runtime>(window: &tauri::WebviewWindow<R>) {
    use webkit2gtk::glib::Cast as _;
    use webkit2gtk::{
        PermissionRequestExt, UserMediaPermissionRequest, UserMediaPermissionRequestExt, WebViewExt,
    };

    let result = window.with_webview(|webview| {
        webview.inner().connect_permission_request(|_, request| {
            let Some(media) = request.downcast_ref::<UserMediaPermissionRequest>() else {
                // Not ours to answer; WebKit's default (deny) stands.
                return false;
            };
            // The camera, and only the camera. One request type covers both
            // devices, so answering it wholesale handed over the microphone
            // too -- a permission nothing in this app asks for. `browser-qr.ts`
            // requests `audio: false`, so a request naming audio is not this
            // app's QR scanner and is refused rather than left to a default.
            //
            // A request for *both* is therefore denied whole, and cannot be
            // otherwise: `PermissionRequest` offers `allow()` and `deny()` and
            // nothing between them, so there is no way to grant the video half
            // and withhold the audio. A future feature wanting both on Linux
            // has to ask twice — once per device — rather than expecting this
            // to split a combined request. Windows and macOS do not share the
            // limitation, so it would present as Linux-only; hence this note
            // rather than leaving it to be rediscovered.
            if media.is_for_video_device() && !media.is_for_audio_device() {
                media.allow();
            } else {
                media.deny();
            }
            true
        });
    });
    if let Err(error) = result {
        eprintln!(
            "could not install the camera permission handler ({error}); QR scanning will not work"
        );
    }
}

/// Everywhere else the webview already resolves this for itself: WebView2
/// prompts, and WKWebView asks a delegate wry answers.
#[cfg(not(target_os = "linux"))]
fn allow_camera_capture<R: tauri::Runtime>(_window: &tauri::WebviewWindow<R>) {}

/// The paths the user has actually dropped on this window.
///
/// This is the whole authorization story for `read_dropped_file`, and the
/// reason the command exists at all rather than the page using the fs plugin's
/// `readFile`. A dropped file can be anywhere on disk, so an fs-plugin route
/// would need a scope covering the entire filesystem — a standing, permanent
/// grant to read any file, held by the webview, to support a gesture. Here the
/// only readable paths are ones the user physically dragged onto this window.
///
/// Bounded rather than cleared per drag. Clearing on each new drop would be
/// tighter, but the page reads the bytes *after* the event, one file at a
/// time, so a second drop landing mid-read would revoke the first drop's paths
/// and lose files the user did drop. The cap keeps the set from growing for
/// the life of the process instead.
#[derive(Default)]
struct DroppedPaths(std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>);

/// How many dropped paths stay readable. Comfortably above `MAX_BATCH_FILES`
/// (10, in `media/attachment-staging.ts`), so a drag is never truncated.
///
/// Gated like the listener that is its only reader. Everywhere else the page
/// handles the drag itself and nothing is ever recorded, so an ungated
/// constant is dead code on Windows and macOS — which is how it was reported.
#[cfg(target_os = "linux")]
const DROPPED_PATHS_REMEMBERED: usize = 64;

/// Record what the user drops, so the page can ask for its bytes.
///
/// Listening for Tauri's own `tauri://drag-drop` rather than reading the drag
/// twice: the page receives the same event and asks for each path over IPC,
/// which is necessarily a later round trip, so the paths are always recorded
/// before the first request for them arrives.
#[cfg(target_os = "linux")]
fn watch_dropped_paths<R: tauri::Runtime>(window: &tauri::WebviewWindow<R>) {
    use tauri::{Listener as _, Manager as _};

    let app = window.app_handle().clone();
    window.listen("tauri://drag-drop", move |event| {
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(event.payload()) else {
            return;
        };
        let Some(paths) = payload.get("paths").and_then(serde_json::Value::as_array) else {
            return;
        };
        let state = app.state::<DroppedPaths>();
        let mut allowed = state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if allowed.len() >= DROPPED_PATHS_REMEMBERED {
            allowed.clear();
        }
        allowed.extend(
            paths
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(std::path::PathBuf::from),
        );
    });
}

/// Everywhere else the page handles the drag itself and already has the bytes;
/// see `main_window` for why the two channels are exclusive.
#[cfg(not(target_os = "linux"))]
fn watch_dropped_paths<R: tauri::Runtime>(_window: &tauri::WebviewWindow<R>) {}

/// Read one file the user dropped on this window.
///
/// Refuses any path that was not dropped, which is what keeps this from being
/// a general "read any file" capability — see `DroppedPaths`. The bytes come
/// back raw (`ipc::Response`) rather than as JSON, so a 20 MB photo crosses the
/// bridge as 20 MB and not as a base64 string half again its size.
///
/// `async` so this does not run on the UI thread. A sync Tauri command runs on
/// the main thread, so reading a multi-gigabyte video — or anything on a slow
/// network mount — would freeze the window for as long as the read took.
///
/// `max_bytes` is checked against the file's *metadata*, before any of it is
/// read. Staging enforces the same limit on the far side, but it only sees a
/// `File` after the whole thing has been read into memory and pushed across
/// the bridge, so an oversized drop would be paid for in full and then
/// rejected. The limit is passed in rather than defined here so that
/// `MAX_UPLOAD_BYTES` stays its single definition: a copy on this side is a
/// copy that drifts.
/// Whether a dropped file is small enough to be worth reading into memory.
///
/// Its own function so the boundary can be tested without standing up a
/// webview, and so it is stated once. The comparison has to agree with the
/// page's: `attachment-staging.ts` refuses a batch only when it goes *over*
/// `MAX_UPLOAD_BYTES`, so a single file of exactly that size is accepted
/// there. Refusing it here would reject a drop the page would have taken.
fn within_upload_limit(size: u64, max_bytes: u64) -> bool {
    size <= max_bytes
}

#[tauri::command(async)]
fn read_dropped_file(
    dropped: tauri::State<'_, DroppedPaths>,
    path: std::path::PathBuf,
    max_bytes: u64,
) -> Result<tauri::ipc::Response, String> {
    let dropped_by_user = dropped
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&path);
    if !dropped_by_user {
        return Err(format!("{} was not dropped on this window", path.display()));
    }
    let size = std::fs::metadata(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?
        .len();
    if !within_upload_limit(size, max_bytes) {
        return Err(format!(
            "{} is {size} bytes, over the {max_bytes} this build will upload",
            path.display()
        ));
    }
    std::fs::read(&path)
        .map(tauri::ipc::Response::new)
        .map_err(|error| format!("could not read {}: {error}", path.display()))
}

/// Claim the OAuth callback scheme with the OS — in development builds only.
///
/// `org.matrixaxon.axon`, per RFC 8252 § 7.1 and ADR 0102 § 4. Note this is
/// *not* `APP_SCHEME`: that one stays `axon`, is served in-webview, and is
/// never registered with the OS.
///
/// A release build must not do this. The installers already register the
/// scheme (`plugins.deep-link.desktop.schemes` is compiled into them, and the
/// generated `.deb` carries `MimeType=x-scheme-handler/org.matrixaxon.axon`),
/// and registering
/// again at runtime writes a *second*, user-level `.desktop` file alongside the
/// installed one. The user is then asked which of two identical-looking
/// handlers should open the link, and the answer decides which binary runs —
/// reported on Linux after installing the `.deb`.
///
/// `is_registered` cannot be used to avoid that: on Linux it only reports
/// whether *this* runtime-written file is the default, so on an installed
/// system it says "no" and the duplicate gets written anyway.
///
/// So development registers itself, because there is no installer in that
/// loop, and release leaves it to the package. This matches the plugin's own
/// model, where desktop deep links belong to installed applications.
///
/// Failure is never fatal. macOS returns `UnsupportedPlatform` by design — the
/// `.app` declares `CFBundleURLTypes` and the OS reads it there — and Linux
/// needs `xdg-mime`, which a minimal container may lack.
fn claim_deep_link_schemes<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if !cfg!(dev) {
        return;
    }
    use tauri_plugin_deep_link::DeepLinkExt;

    if let Err(error) = app.deep_link().register_all() {
        log_scheme_registration(&error);
    }
}

/// Split out so the message is written once and the reason is stated.
fn log_scheme_registration(error: &tauri_plugin_deep_link::Error) {
    if matches!(error, tauri_plugin_deep_link::Error::UnsupportedPlatform) {
        // macOS: the bundle declares the scheme, so there is nothing to do and
        // nothing has gone wrong.
        return;
    }
    // The scheme is not named here: it comes from `tauri.conf.json`, and a
    // message that repeats a hard-coded one is a message that can disagree
    // with what was actually attempted — which is how this line read before.
    eprintln!("could not register the deep-link scheme for development ({error})");
}

/// Create the app window.
///
/// Built here rather than declared in `tauri.conf.json` because its URL has to
/// differ between dev and release, and static JSON cannot say that.
///
/// A release build loads `APP_SCHEME`, so `serve` below can fall an unknown
/// path back to the app. A dev build must load `WebviewUrl::App`, which Tauri
/// resolves to `devUrl` — the Vite server, with hot reload. Naming the custom
/// scheme in the config instead looks like it works and quietly costs the whole
/// dev loop: an absolute non-http URL is treated as external, so it overrides
/// `devUrl`, `tauri dev` stops using Vite at all, and the window silently
/// serves whatever `dist/` was last built. Worse, `cargo run` on a fresh
/// checkout — where `dist/` does not exist, because it is gitignored — then
/// renders `serve`'s "bundle is missing" error instead of the app.
fn main_window<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> tauri::Result<tauri::WebviewWindow<R>> {
    let url = if cfg!(dev) {
        tauri::WebviewUrl::App("index.html".into())
    } else {
        tauri::WebviewUrl::CustomProtocol(
            format!("{APP_SCHEME}://localhost/")
                .parse()
                .expect("app scheme URL"),
        )
    };
    let builder = tauri::WebviewWindowBuilder::new(app, "main", url)
        .title("Axon")
        .inner_size(1100.0, 760.0)
        .min_inner_size(380.0, 480.0)
        // The window stays on the app's own origin, and this is the only thing
        // that says so. The CSP does not: `default-src 'self'` constrains where
        // resources are *fetched* from, not where the top-level document may
        // *navigate* — there is no CSP directive for that at all, `navigate-to`
        // having never shipped. Without this the shell would be one stray
        // `location =` away from rendering somebody else's page in a window
        // holding this app's camera grant, its capability set and its tokens,
        // with no address bar to show it had happened.
        //
        // In-app routing is untouched: the client routes with `pushState`,
        // which is not a navigation, and the one real load it performs
        // (`disconnectFromServer`'s reload to `/`) is same-origin.
        //
        // External links never arrive here — `openExternal` hands them to the
        // real browser (`app.tsx`) — so anything that does reach this point is
        // something no code path intends, which is exactly what to refuse.
        .on_navigation(|target| navigation_allowed(target, cfg!(dev)));
    // Which process handles a file drag, and it cannot be both.
    //
    // Left enabled, Tauri swallows the drop and the page's own HTML5
    // drag-and-drop never fires. Windows *requires* it disabled for HTML5
    // drag-and-drop to work at all — the plugin's own documentation says so of
    // `dragDropEnabled`, and it claims no such requirement elsewhere — and
    // macOS works with it disabled too, so both keep the page's channel.
    //
    // Linux cannot use that channel at all. WebKitGTK advertises
    // `text/uri-list` for a file-manager drag and puts no `File` behind it, so
    // `dataTransfer.files` is empty and there is nothing for the page to stage:
    // the drop was accepted and then did nothing, which is what was reported.
    // Handling the drag here is the only way to get at the file, and
    // `read_dropped_file` is how the page then reads it.
    #[cfg(not(target_os = "linux"))]
    let builder = builder.disable_drag_drop_handler();
    builder.build()
}

/// Whether the window may navigate to `target`.
///
/// A pure function over `(url, is this a dev build?)` so the rule can be
/// asserted without a webview — it is a security boundary, and the alternative
/// is finding out from a packaged build.
fn navigation_allowed(target: &tauri::Url, dev: bool) -> bool {
    if target.scheme() == APP_SCHEME {
        return true;
    }
    // A dev build loads `devUrl`, the Vite server, instead. Release admits
    // nothing but the app's own scheme — not http, not localhost, not the
    // configured Axon server, which is reached by `fetch` and never navigated
    // to.
    dev && matches!(target.host_str(), Some("localhost" | "127.0.0.1"))
}

/// The environment variable that decides whether WebKitGTK uses its DMA-BUF
/// renderer. Named once so the code and its test cannot drift apart.
#[cfg(target_os = "linux")]
const DMABUF_RENDERER_VAR: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";

/// Stop WebKitGTK losing `<canvas>` content on Linux.
///
/// A PDF opened in the app showed a correctly sized, correctly paginated,
/// entirely **blank** page. pdf.js was not at fault: it fetched the document,
/// reported the right page count, and its render task resolved successfully
/// with no error. The pixels simply never arrived on screen. Confirmed against
/// the real file — decrypted out of the room and rasterised with poppler, which
/// draws it — and against the same `dist` in Chromium, which also draws it.
///
/// The cause is WebKitGTK's DMA-BUF renderer, the path it uses to hand painted
/// buffers from the web process to the UI process. Where that path misbehaves,
/// what arrives is empty rather than wrong, and nothing anywhere reports an
/// error — which is why this was invisible until someone opened a PDF.
///
/// Confirmed on a real GPU-backed desktop as well as a VNC session, so this is
/// not an artifact of software rendering and cannot be left to the display
/// setup. Windows and macOS are unaffected: their engines never take this path,
/// and both render the same file correctly today.
///
/// Deliberately the narrow flag. `WEBKIT_DISABLE_COMPOSITING_MODE` also fixes
/// it, by turning off hardware-accelerated compositing for the entire webview —
/// which would tax scrolling and animation everywhere to fix one buffer
/// handoff. This disables only the handoff that is broken.
///
/// Set rather than forced, so anyone whose system does not need it can put the
/// renderer back with `WEBKIT_DISABLE_DMABUF_RENDERER=0` in the environment.
///
/// Safe to mutate the environment here: this runs at the top of `run()`, before
/// Tauri starts anything, so the process is still single-threaded and nothing
/// else can be reading it.
#[cfg(target_os = "linux")]
fn keep_canvas_content() {
    if std::env::var_os(DMABUF_RENDERER_VAR).is_none() {
        std::env::set_var(DMABUF_RENDERER_VAR, "1");
    }
}

/// The scheme the production build is served from.
///
/// Tauri's built-in asset protocol has no SPA fallback and cannot be
/// overridden, so the shell serves the bundle itself under its own scheme.
/// On Windows and Android this surfaces as `http://axon.localhost`.
const APP_SCHEME: &str = "axon";

/// Build the response for one resolved asset.
///
/// Split out from `serve` so the headers can be asserted directly. The CSP is
/// configured in `tauri.conf.json`, and Tauri applies it by *serving* it: the
/// asset resolver computes the header — including the per-load nonces the
/// policy refers to — and expects whoever answers the request to send it.
/// Answering with only the bytes, which this did, meant the production build
/// enforced no policy at all, and every reason the policy exists (one origin,
/// no third-party script, no remote frames) held only in the config file.
///
/// The tests below covered `route`, a pure function over paths, which is why
/// nothing caught a missing header.
fn asset_response(
    mime_type: &str,
    csp: Option<&str>,
    bytes: Vec<u8>,
) -> tauri::http::Response<Vec<u8>> {
    let mut builder = tauri::http::Response::builder()
        .status(tauri::http::StatusCode::OK)
        .header(tauri::http::header::CONTENT_TYPE, mime_type);
    if let Some(csp) = csp {
        builder = builder.header(tauri::http::header::CONTENT_SECURITY_POLICY, csp);
    }
    builder.body(bytes).expect("asset response")
}

/// What to answer for one request path.
///
/// Deliberately a pure function over `(how do I resolve an asset?, path)` so
/// the rules can be asserted without a webview — see the tests below. Generic
/// in what resolution produces so that `serve` can hand it the real asset and
/// the tests a name; either way the lookup happens once, and the asset the
/// route decided on is the asset that gets served.
#[derive(Debug, PartialEq, Eq)]
enum Route<A> {
    /// Serve this asset.
    Asset(A),
    /// Serve the app; the client router reads the path.
    App,
    /// A content-hashed asset that is genuinely gone.
    NotFound,
}

/// Answer one request out of the embedded bundle.
fn serve<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    path: &str,
) -> tauri::http::Response<Vec<u8>> {
    let resolver = app.asset_resolver();
    let builder = tauri::http::Response::builder();

    // Resolved once. The resolver decodes the asset and mints a fresh CSP nonce
    // on every call (see `asset_response`), so asking whether an asset exists
    // and then asking for it again paid that twice for every script, style,
    // font and image the app loads.
    let asset = match route(path, |candidate| resolver.get(candidate.into())) {
        Route::Asset(asset) => Some(asset),
        Route::App => resolver.get("index.html".into()),
        Route::NotFound => {
            return builder
                .status(tauri::http::StatusCode::NOT_FOUND)
                // A miss under /assets/ must not be cached: a client that asked
                // one moment too early during a rollout would otherwise keep
                // not-finding it. Same reasoning as deploy/web/Caddyfile.
                .header("Cache-Control", "no-store")
                .header("Content-Type", "text/plain")
                .body(b"not found\n".to_vec())
                .expect("static 404 response");
        }
    };

    match asset {
        Some(asset) => asset_response(&asset.mime_type, asset.csp_header.as_deref(), asset.bytes),
        // The bundle is empty. `generate_context!` embeds `../dist` at compile
        // time and says nothing when it is not there -- and `dist/` is
        // gitignored, so a fresh checkout has none. Building the Rust crate
        // directly (`cargo build`/`cargo run`) therefore produces a binary that
        // compiles, launches, and renders only this. Say what to do about it.
        None => builder
            .status(tauri::http::StatusCode::INTERNAL_SERVER_ERROR)
            .header("Content-Type", "text/plain")
            .body(
                concat!(
                    "No web bundle is embedded in this binary.\n\n",
                    "`dist/` is generated and gitignored, and it is baked in at ",
                    "compile time, so building this crate on its own produces an ",
                    "empty app. Build through the Tauri CLI, which runs the ",
                    "frontend build first:\n\n",
                    "    cd clients/web && pnpm install && pnpm tauri build\n\n",
                    "or, for a dev loop with hot reload:\n\n",
                    "    cd clients/web && pnpm tauri dev\n",
                )
                .as_bytes()
                .to_vec(),
            )
            .expect("static 500 response"),
    }
}

/// Route one request, mirroring `deploy/web/Caddyfile` exactly.
///
/// Two rules, and the second is the subtle one:
///
/// - An unknown path serves `index.html`, because the deep-link URL shape
///   `/:accountId/rooms/:roomId` is a routing contract, not a file
///   (ADR 0046 open question 5, settled by ADR 0102 § 5). Caddy spells this
///   `try_files {path} /index.html`.
/// - An unknown path *under `/assets/`* is a 404 instead. Those filenames carry
///   a content hash, so a miss is never a route — it is a chunk a redeploy
///   deleted. Answering it with `index.html` and a 200 makes the browser parse
///   HTML as a module, and a client still running the previous build hangs with
///   no useful error. That is the exact failure ADR 0087 exists to fix, and the
///   Caddyfile carries the same carve-out for the same reason.
fn route<A>(path: &str, resolve: impl Fn(&str) -> Option<A>) -> Route<A> {
    let trimmed = path.trim_start_matches('/');
    let candidate = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };

    if let Some(asset) = resolve(candidate) {
        return Route::Asset(asset);
    }
    if candidate.starts_with("assets/") {
        return Route::NotFound;
    }
    Route::App
}

#[cfg(test)]
mod tests {
    use super::{asset_response, navigation_allowed, route, within_upload_limit, Route};

    /// The bug this covers shipped: `serve` answered with the bytes and the
    /// content type and dropped the policy, so the release build enforced no
    /// CSP at all while `tauri.conf.json` said it did.
    #[test]
    fn an_asset_response_carries_the_configured_csp() {
        let response = asset_response(
            "text/html",
            Some("default-src 'self'"),
            b"<!doctype html>".to_vec(),
        );

        assert_eq!(
            response
                .headers()
                .get(tauri::http::header::CONTENT_SECURITY_POLICY)
                .map(|v| v.to_str().expect("ascii header")),
            Some("default-src 'self'"),
        );
        assert_eq!(
            response
                .headers()
                .get(tauri::http::header::CONTENT_TYPE)
                .map(|v| v.to_str().expect("ascii header")),
            Some("text/html"),
        );
    }

    /// The page accepts a single file of exactly `MAX_UPLOAD_BYTES`, so the
    /// shell has to as well: an off-by-one here would refuse a drop that the
    /// very next check would have allowed, and the user would see a file
    /// silently skipped with no cap having actually been exceeded.
    #[test]
    fn a_file_at_the_limit_is_read_and_one_byte_more_is_not() {
        assert!(within_upload_limit(0, 100));
        assert!(within_upload_limit(100, 100));
        assert!(!within_upload_limit(101, 100));
    }

    /// Not every asset has one — the resolver returns `None` for anything that
    /// is not the document — and a literal "None" header would be worse than
    /// no header.
    #[test]
    fn an_asset_without_a_policy_gets_no_header() {
        let response = asset_response("image/png", None, b"\x89PNG".to_vec());

        assert!(response
            .headers()
            .get(tauri::http::header::CONTENT_SECURITY_POLICY)
            .is_none());
    }

    fn url(raw: &str) -> tauri::Url {
        raw.parse().expect("test url")
    }

    /// The window is the app's origin and nothing else. It holds the camera
    /// grant, the capability set and the session, and has no address bar to
    /// show that it is somewhere unexpected.
    #[test]
    fn a_release_window_navigates_only_to_the_app_scheme() {
        assert!(navigation_allowed(&url("axon://localhost/"), false));
        assert!(navigation_allowed(
            &url("axon://localhost/@a:b/rooms/!c:d"),
            false
        ));

        assert!(!navigation_allowed(&url("https://evil.example/"), false));
        assert!(!navigation_allowed(&url("http://localhost:5173/"), false));
        // The configured Axon server is reached by fetch, never navigated to.
        assert!(!navigation_allowed(
            &url("https://axon.example/v1/rooms"),
            false
        ));
        assert!(!navigation_allowed(&url("file:///etc/passwd"), false));
        assert!(!navigation_allowed(
            &url("data:text/html,<script>1</script>"),
            false
        ));
    }

    /// `tauri dev` serves from Vite, so the same rule would lock the dev loop
    /// out of its own window.
    #[test]
    fn a_dev_window_also_admits_the_vite_server() {
        assert!(navigation_allowed(&url("http://localhost:5173/"), true));
        assert!(navigation_allowed(&url("http://127.0.0.1:5173/"), true));
        // Still nothing else, even in dev.
        assert!(!navigation_allowed(&url("https://evil.example/"), true));
    }

    /// One test, not two: the environment is process-wide and `cargo test`
    /// runs tests on parallel threads, so splitting these would let them race
    /// each other over the same variable.
    #[cfg(target_os = "linux")]
    #[test]
    fn dmabuf_renderer_is_disabled_unless_the_user_chose_otherwise() {
        use super::{keep_canvas_content, DMABUF_RENDERER_VAR};

        // SAFETY-adjacent: this is the only test that touches the environment,
        // so nothing else can observe it mid-change.
        std::env::remove_var(DMABUF_RENDERER_VAR);
        keep_canvas_content();
        assert_eq!(
            std::env::var(DMABUF_RENDERER_VAR).as_deref(),
            Ok("1"),
            "an unset variable must be filled in, or WebKitGTK loses canvas content"
        );

        // Someone who does not need this must be able to put the renderer back.
        std::env::set_var(DMABUF_RENDERER_VAR, "0");
        keep_canvas_content();
        assert_eq!(
            std::env::var(DMABUF_RENDERER_VAR).as_deref(),
            Ok("0"),
            "an explicit choice must survive"
        );
        std::env::remove_var(DMABUF_RENDERER_VAR);
    }

    /// Stands in for the embedded bundle, resolving a name to itself.
    fn bundle(path: &str) -> Option<String> {
        matches!(
            path,
            "index.html" | "assets/index-abc123.js" | "favicon.png" | "version.json"
        )
        .then(|| path.to_string())
    }

    #[test]
    fn serves_a_real_asset() {
        assert_eq!(
            route("/assets/index-abc123.js", bundle),
            Route::Asset("assets/index-abc123.js".into())
        );
        assert_eq!(
            route("/favicon.png", bundle),
            Route::Asset("favicon.png".into())
        );
    }

    #[test]
    fn serves_the_app_at_the_root() {
        assert_eq!(route("/", bundle), Route::Asset("index.html".into()));
        assert_eq!(route("", bundle), Route::Asset("index.html".into()));
    }

    #[test]
    fn a_deep_route_serves_the_app() {
        // The whole point. Verified against the real resolver first: it returns
        // nothing for these, so without this rule a reload at a room URL —
        // which `stores/update-check.ts` performs on its own — would 404 the
        // app out of existence.
        assert_eq!(
            route("/@alice:example.org/rooms/!abc:example.org", bundle),
            Route::App
        );
        assert_eq!(route("/settings", bundle), Route::App);
        assert_eq!(route("/oauth/callback", bundle), Route::App);
    }

    #[test]
    fn a_missing_hashed_chunk_is_an_honest_404() {
        // ADR 0087. Serving index.html here would hand a stale client HTML
        // where it expected a module, and it would hang rather than reload.
        assert_eq!(route("/assets/index-deleted.js", bundle), Route::NotFound);
        assert_eq!(route("/assets/style-gone.css", bundle), Route::NotFound);
    }
}
