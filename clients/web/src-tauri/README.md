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

`icons/` is 52 files generated from the shared master in the browser client:

```sh
cd clients/web && pnpm exec tauri icon public/favicon.svg -o src-tauri/icons
```

**One artwork file serves both clients.** `clients/web/public/favicon.svg` is
the browser's favicon, loaded directly by `index.html`, and it is what this set
is rasterised from. A second copy here is the drift problem in waiting, and
this directory has already had it once: the committed set was generated from a
source two commits behind the one sitting beside it, so all 52 files disagreed
with their own source and the whole set was opaque where the source was not.

The master is an SVG on purpose. `tauri icon` takes one directly, a browser
takes one directly, it rasterises crisply from 16px to the 1024px the App Store
inspects, and it is a text file — so a change to the artwork reads as a diff
rather than an opaque binary blob. Keep it to plain geometry: strokes and
paths, one colour, no text, no filters, no external references. `tauri icon`
rasterises with resvg rather than a browser engine, and anything beyond flat
geometry is where renderers start to disagree.

Both generated sets are committed, and the `icons-regenerated` pre-push hook
refuses a master change that does not regenerate both — `tauri icon` for this
directory and `scripts/build-web-icons.sh` for the browser's PNGs. Nothing in
either build reads the master except as a static asset, so a forgotten
regeneration is silent.

The hook checks only that they changed together, not that the output matches.
`tauri icon` is not reproducible: two runs over one source give 51
byte-identical files and an `icon.icns` whose members come out in a different
order each time. That is also why generation is not part of the build — every
release would otherwise carry a different `.icns` than the last, for no reason,
and macOS notarization eventually signs over those bytes.

Transparency is kept for Linux, Windows and macOS, which all draw a _shaped_
icon — `icon.icns` and `icon.ico` both carry it. iOS is the exception: it wants
a full-bleed square and masks its own corners, so `tauri icon` composites an
opaque background for that set alone. Its white default is deliberate. The
obvious-looking `--ios-color '#5142E6'` paints the background in the same
colour as the glyph and yields a plain blue square.

Note the iOS files still carry an _alpha channel_ even though nothing in them
is transparent. App Store Connect rejects an app icon with one at all
(`ITMS-90717`), so they need flattening to RGB before submission. No
`tauri icon` flag does it — `--ios-color` changes the colour and still writes
RGBA — so it needs a post-processing step. Tracked as #410 for M-W13 rather
than built here, where nothing consumes `icons/ios/` yet.

**32px is the ceiling on detail**, not 1024. A Linux launcher and a Windows
taskbar draw it at 32, and every further branch costs separation there first.
Look at `icons/32x32.png` before adding one.

Note `icons/icon.png` and `icons/64x64.png` are emitted by the generator but
referenced by nothing here; `icons/android/` and `icons/ios/` are for M-W13.
Editing any of them has no effect on a desktop build, and the next regeneration
overwrites them.

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

## The glib advisory is not actionable here

Dependabot reports **GHSA-wrw7-89jp-8q8g** against this workspace's
`Cargo.lock`: unsoundness in `glib::VariantStrIter`'s `Iterator` impls,
affecting `>=0.15.0, <0.20.0`, fixed in 0.20.0. The lock carries 0.18.5.

It has no CVE and no RUSTSEC alias, so `cargo deny check advisories` — whose
database is RustSec's — does not report it, on this workspace or any other.
GitHub's advisory database is the only one that carries it.

**It cannot be upgraded from here, and the block is not our dependency.** Every
GTK binding in the graph caps glib at `^0.18`:

```
$ cargo update -p glib --precise 0.20.9
error: failed to select a version for the requirement `glib = "^0.18.0"`
  candidate versions found which didn't match: 0.20.9
  required by package `webkit2gtk v2.0.2`
```

`webkit2gtk 2.0.2` is the latest published version of that crate, and it is not
alone: `gtk`, `gio`, `atk`, `gdk`, `gdk-pixbuf`, `gdkx11`, `pango`, `soup3` and
`javascriptcore-rs` all require `^0.18`, and they arrive through `tao` and
`wry` — Tauri's own dependencies. Moving glib means moving Tauri's whole GTK
stack to gtk-rs 0.20, which is upstream work. No lockfile edit, version bump or
`[patch]` reaches it, and forcing it would mean vendoring a fork of the binding
stack to fix an unsoundness nothing here calls.

The affected API iterates GVariant string arrays. This crate touches glib in
exactly one place — `use webkit2gtk::glib::Cast as _` in the permission
handler — and `VariantStrIter` appears nowhere in `tao`, `wry`, `gtk`, `gio` or
`webkit2gtk`. It is a soundness bug requiring that iterator to be used, not an
attacker-reachable input path.

Tracked in #413; recheck when Tauri bumps its GTK bindings. Note the root
workspace has no glib at all, so this is the desktop shell alone.
