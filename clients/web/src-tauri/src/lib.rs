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

/// Wire up and run the shell.
///
/// `pub` and in the library rather than `main.rs` because the mobile targets
/// (M-W13) link this crate and call in through their own generated entry
/// point; the desktop binary is a one-line caller of the same function.
pub fn run() {
    tauri::Builder::default()
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
        // Serve the bundle ourselves, so an unknown path can fall back to the
        // app instead of 404ing. See `route`.
        .register_uri_scheme_protocol(APP_SCHEME, |ctx, request| {
            serve(ctx.app_handle(), request.uri().path())
        })
        .setup(|app| {
            main_window(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Axon shell");
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
    tauri::WebviewWindowBuilder::new(app, "main", url)
        .title("Axon")
        .inner_size(1100.0, 760.0)
        .min_inner_size(380.0, 480.0)
        // Tauri swallows OS file drops by default, which would silently break
        // `media/use-file-drop.ts` — dropping a file on a room would do nothing.
        .disable_drag_drop_handler()
        .build()
}

/// The scheme the production build is served from.
///
/// Tauri's built-in asset protocol has no SPA fallback and cannot be
/// overridden, so the shell serves the bundle itself under its own scheme.
/// On Windows and Android this surfaces as `http://axon.localhost`.
const APP_SCHEME: &str = "axon";

/// What to answer for one request path.
///
/// Deliberately a pure function over `(does this asset exist?, path)` so the
/// rules can be asserted without a webview — see the tests below.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// Serve this asset.
    Asset(String),
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

    let target = match route(path, |candidate| resolver.get(candidate.into()).is_some()) {
        Route::Asset(asset) => asset,
        Route::App => "index.html".to_string(),
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

    match resolver.get(target.clone()) {
        Some(asset) => builder
            .status(tauri::http::StatusCode::OK)
            .header("Content-Type", asset.mime_type)
            .body(asset.bytes)
            .expect("asset response"),
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
fn route(path: &str, exists: impl Fn(&str) -> bool) -> Route {
    let trimmed = path.trim_start_matches('/');
    let candidate = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };

    if exists(candidate) {
        return Route::Asset(candidate.to_string());
    }
    if candidate.starts_with("assets/") {
        return Route::NotFound;
    }
    Route::App
}

#[cfg(test)]
mod tests {
    use super::{route, Route};

    /// Stands in for the embedded bundle.
    fn bundle(path: &str) -> bool {
        matches!(
            path,
            "index.html" | "assets/index-abc123.js" | "favicon.png" | "version.json"
        )
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
