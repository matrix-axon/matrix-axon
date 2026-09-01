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

`icons/` is 52 files generated from `icon-source.svg`:

```sh
cd clients/web && pnpm exec tauri icon src-tauri/icon-source.svg -o src-tauri/icons
```

**The source is an SVG on purpose.** `tauri icon` takes one directly, and a
vector master costs nothing and settles two things a raster one cannot: it
rasterises crisply at every size from 16px to the 1024px the App Store
inspects, and it is a text file, so a change to it is legible in review
instead of an opaque binary blob.

Keep it to plain geometry — strokes and paths, one colour, no text, no filters,
no external references. `tauri icon` rasterises with resvg rather than a
browser engine, and anything beyond flat geometry is where renderers start to
disagree. Text in particular depends on fonts the rasteriser may not have.

The artwork is a neuron composed on the square's diagonal: dendrites at lower
left, the soma, then an axon running up and right and arborising into three
terminals, the upper of which branches again. Diagonal because an app icon is a
square and a horizontally-composed mark wastes most of it — the mark this
replaced spanned 93% of the width and 56% of the height, which at 32px left a
thin strip floating in empty space.

Two stroke weights keep it from reading as a diagram: the soma and axon carry
the mass at 104, the dendrites and terminals taper to 80. Branch points are
staggered and the angles uneven on purpose — evenly spaced branches of equal
length read as a snowflake rather than a cell.

**32px is the ceiling on detail**, not 1024. A Linux launcher and a Windows
taskbar draw it at 32, and every further branch costs separation there first.
Look at `icons/32x32.png` before adding one.

It is transparent, which is what Linux, Windows and macOS all want: those show
a _shaped_ icon, and `icon.icns` and `icon.ico` both carry real transparency.
iOS is the exception — it wants a full-bleed square and masks its own corners —
and `tauri icon` composites an opaque background for that set alone. The
default is white, which is deliberate here: the obvious-looking
`--ios-color '#5142E6'` paints the background in the same colour as the glyph
and yields a plain blue square.

Note the iOS files still carry an _alpha channel_ even though nothing in them
is transparent. App Store Connect rejects an app icon with one at all
(`ITMS-90717`), so they need flattening to RGB before submission. No
`tauri icon` flag does it — `--ios-color` changes the colour and still writes
RGBA — so it needs a post-processing step. Tracked as #410 for M-W13 rather
than built here, where nothing consumes `icons/ios/` yet.

Both sides are committed, and the `icons-regenerated` pre-push hook refuses a
change to `icon-source.svg` that does not regenerate them — the five files
`tauri.conf.json` bundles all live under `icons/`, nothing in the build reads
the source, so a forgotten regeneration silently ships the old artwork. That
had already happened once: the source was replaced and the icons were not.

The hook checks only that the two changed together, not that the output
matches. `tauri icon` is not reproducible: two runs over one source give 51
byte-identical files and an `icon.icns` whose members come out in a different
order each time. That is also why generation is not part of the build — every
release would otherwise carry a different `.icns` than the last, for no reason,
and macOS notarization eventually signs over those bytes.

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
