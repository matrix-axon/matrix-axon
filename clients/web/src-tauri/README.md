# axon-desktop

The native shell around the `clients/web` bundle (ADR 0102, M-W12). Desktop
today; iOS and Android are M-W13.

## Build it through the Tauri CLI, not cargo

```sh
cd clients/web
pnpm install
pnpm tauri dev     # dev loop, hot reload
pnpm tauri build   # release binary + installers
```

**`cargo build` / `cargo run` in this directory produce a binary that launches
and shows nothing but an error.** That is not a broken checkout. The frontend is
embedded at compile time from `../dist`, which is generated and gitignored, so
a fresh clone has none — and `tauri::generate_context!()` says nothing about it.
The CLI is what runs the frontend build first (`beforeBuildCommand`); cargo on
its own has no idea it needs to. The binary explains this if you hit it.

## Its own cargo workspace

Not a member of the repo root's. `cross-build.yml` runs `cargo build
--workspace` on three platforms and the pre-push gate runs `cargo clippy
--all-targets`; membership would have put webkit2gtk and a desktop app build on
every one of them. The root `Cargo.toml` names this directory in `exclude` so
cargo treats it as deliberate rather than an unlisted member.

The pre-push gate covers it separately, as `shell-fmt`, `shell-clippy` and
`shell-test`, filtered to this directory.

## Linux build dependencies

```sh
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

Only needed if you actually touch this crate; nothing else in the repo links
against them.

## The dev server port is pinned

`beforeDevCommand` is `pnpm dev --strictPort`, so it fails if 5173 is taken
rather than drifting to 5174. `devUrl` in `tauri.conf.json` names 5173, and
Vite's default of quietly picking another port means the shell would otherwise
load whatever _else_ is on 5173 — a different app, with no error anywhere.

## Linux desktop entry

`bundle/axon.desktop` overrides Tauri's built-in template, and exists for one
character: the `%u` on `Exec`.

`%u` is the freedesktop field code that passes a URL to the program. Without
it the launcher starts the app with _no argument_, so an
`org.matrixaxon.axon:/oauth/callback?...` link resolves to this entry,
launches the app, and
the URL is silently dropped — the sign-in completes in the browser and the app
never hears about it. Tauri's default template has no field code, because
nothing tells it this app handles a URL scheme; the `deep-link` plugin
contributes the `MimeType` line but not the argument. The two halves have to
agree or the association is decoration.

`Name` is fixed rather than `{{name}}`. That variable is `productName`, which
also names the package (`axon-desktop`), and a launcher should show the app's
name rather than the package's.

## Icons

`icons/` is generated from `icon-source.png`, which is the artwork as
supplied. It is currently **60×60**, and `tauri icon` wants 1024×1024, so it is
upscaled on the way in:

```sh
python3 -c "from PIL import Image; \
  Image.open('src-tauri/icon-source.png').convert('RGBA') \
    .resize((1024,1024), Image.LANCZOS).save('/tmp/axon-1024.png')"
pnpm exec tauri icon /tmp/axon-1024.png -o src-tauri/icons
```

A 17× upscale cannot add detail. It is fine at the sizes a window and a
launcher actually use, and soft at 1024 — which is the size the App Store
requires and inspects. **Replace `icon-source.png` with artwork at 1024×1024
(or the original vector) before any store submission**, and regenerate; nothing
else has to change.

## The bundle identifier is settled

`org.matrixaxon.axon`, confirmed for ADR 0102 § 4. It is a permanent store
identity and cannot be changed later without shipping a new application, so
treat it as fixed rather than as a default to revisit.

It is also the OAuth callback scheme — the callback is
`org.matrixaxon.axon:/oauth/callback` — which is why a reverse-domain
identifier rather than a short one matters beyond the stores: a private-use
scheme is claimed first-come and unauthenticated on every desktop OS, so
anything registering a bare `axon` could receive an authorization code meant
for this app (RFC 8252 § 8.4, § 8.6).

## Sign-in needs an entry on the server

A build of this crate cannot sign in against a server that has not registered
it. The shell identifies itself as `axon-desktop` with the callback
`org.matrixaxon.axon:/oauth/callback`, and the server allow-lists that pair
exactly — see "SSO sign-in" in `../README.md` for the `[[oauth.clients]]`
entry and the three ways it is commonly wrong.

Note the OAuth callback scheme is not the one the bundle is served from.
`APP_SCHEME` in `src/lib.rs` stays `axon`: that is an in-webview protocol
handler, never registered with the OS, and takes no part in OAuth. The OAuth
scheme is named in three places with no shared source — `OAUTH_CLIENT` in
`../src/platform/tauri.ts`, `plugins.deep-link.desktop.schemes` in
`tauri.conf.json`, and the operator's `redirect_uris` — so changing one alone
produces a sign-in that dead-ends in the browser.
