# Axon desktop shell

The native shell around the `clients/web` bundle (ADR 0102). Desktop is M-W12,
iOS is M-W13 (see [iOS](#ios) below); Android is not built yet.

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

## macOS: build for both architectures

`tauri build` targets the host, so a build on Apple Silicon produces an arm64
bundle that **will not open on an Intel Mac**. (An x86_64 bundle does run on
Apple Silicon, under Rosetta 2 — the failure is one-directional, which makes it
easy to miss when testing on the newer machine.)

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
pnpm tauri build --target universal-apple-darwin
```

Both slices are lipo'd into one bundle. The Intel slice cross-compiles from
Apple Silicon; no second machine is needed, and the webview is a system
framework (WKWebView), so there is no per-architecture native library to
supply.

Note this is the _opposite_ of what `.github/workflows/cross-build.yml` does
for the server and TUI, which ship per-arch zips on purpose. ADR 0102 § 9 has
the reasoning for the divergence.

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
is now `Axon` and would give the same answer — but it also names the deb
package and the macOS bundle, so pinning the launcher's `Name` keeps a future
packaging rename out of what the user reads in their menu.

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
directory and `scripts/build-brand-assets.py` for the browser's PNGs. Nothing in
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
RGBA — so it needs a post-processing step. `scripts/package-ios.sh` is that
step: it flattens the set onto white as it copies it into the generated Xcode
project, via `scripts/lib/flatten-icons.swift`. The committed files under
`icons/ios/` keep their alpha channel, because they are what `tauri icon`
produces and regenerating must not show a diff.

**32px is the ceiling on detail**, not 1024. A Linux launcher and a Windows
taskbar draw it at 32, and every further branch costs separation there first.
Look at `icons/32x32.png` before adding one.

Note `icons/icon.png` and `icons/64x64.png` are emitted by the generator but
referenced by nothing here, and `icons/android/` is unused until Android is
built. `icons/ios/` is the artwork of record for the iOS app — read by
`scripts/package-ios.sh`, not by the desktop build. Editing any of them by hand
has no effect and the next regeneration overwrites them; change
`icons/icon.svg` and re-run the generator instead.

## iOS

Needs macOS with Xcode. Everything below is `clients/web` unless it says
otherwise.

### The dev loop runs on the phone, not on localhost

```sh
cd clients/web
pnpm tauri ios dev
```

On a phone, `localhost` is the phone. The CLI picks a LAN address for the dev
server and exports it as `TAURI_DEV_HOST`; `vite.config.ts` reads that to bind
the right interface, to point the HMR socket back at this machine, and to
allow that Host header. Without it the CLI waits forever on
`http://<lan-ip>:5173/` with nothing listening there.

What `TAURI_DEV_HOST` switches is exactly those three things — `host`, `hmr`
and `allowedHosts` in `vite.config.ts` — and nothing else. So **do not export
it by hand for desktop work**, and never export it empty: `''` is Vite's "bind
every interface", which publishes the dev server — and whatever session it is
signed into — to the whole network. `vite.config.ts` treats an empty value as
absent for exactly that reason.

Two neighbouring pieces of the mobile loop are switched by something else, and
looking for them here is how an afternoon goes missing:

- **The dev server shuts itself down when its launcher exits.** The
  `axon-exit-when-orphaned` Vite plugin records every ancestor pid at startup
  and exits once any of them is gone, so 5173 and 1421 are released instead of
  being held by a server whose CLI has died — which otherwise makes the next
  `tauri ios dev` fail on a port that looks busy for no reason. It is armed by
  `TAURI_ENV_PLATFORM`, which the Tauri CLI sets, so it runs under desktop
  `pnpm tauri dev` as well as `tauri ios dev`, and never under a plain
  `pnpm dev`, a `nohup pnpm dev > log &`, or vitest.
- **The navigation guard** in `src-tauri/src/lib.rs` reads the dev host from
  `app.config().build.dev_url`, which the CLI compiles into the config it
  builds, not from the environment variable.

Being on a LAN address also means the axon server has to be reachable from the
phone. `localhost:8080` is not; use the machine's LAN name or a Tailscale
address in the app's server setting.

### Build, install and upload with `scripts/package-ios.sh`

```sh
scripts/package-ios.sh --install                    # to a connected device
scripts/package-ios.sh --export-method app-store-connect \
  --build-number 2 --upload                         # to TestFlight
```

`pnpm tauri ios build` on its own does not produce a shippable app from a
clean checkout, and none of the ways it falls short announce themselves. The
script's own header lists them; the short version is that `gen/apple` is
generated once and then frozen, so it regenerates it every run and passes
`bundle.iOS` settings as `--config` overrides; that `tauri icon`'s iOS set
carries an alpha channel App Store Connect rejects, so it flattens it on the
way in; and that a Homebrew `rust` on `PATH` shadows rustup and has no iOS
`std`, so it puts `~/.cargo/bin` first and then checks; and that an exported
`FORCE_COLOR=1` (common in a shell rc) makes Xcode's Rust build phase read the
`1` as an architecture and fail with
`Arch specified by Xcode was invalid. {arch} isn't a known arch`, so it unsets
it. If you call `pnpm tauri ios build` yourself from such a shell, `unset
FORCE_COLOR` first.

`--upload` needs `ASC_KEY_ID` and `ASC_ISSUER_ID`, and an
`~/.appstoreconnect/private_keys/AuthKey_*.p8`. Both are checked before the
build rather than after it.

There is no CI lane — [#445](https://github.com/matrix-axon/matrix-axon/issues/445)
tracks one, and this script is what it should be built from.

### Sign in with Apple needs the entitlement and a matching profile

The iOS app signs in with Apple natively (ADR 0054), through the in-repo
`native-auth` plugin, which also keeps Axon's tokens in the Keychain. Two things
have to agree before the Apple sheet will open:

- **The App ID has the capability.** In the developer portal, under
  Identifiers, `org.matrixaxon.axon` needs Sign in with Apple enabled, and
  enabled as a primary App ID. Any Services ID used for browser sign-in should
  be grouped under it (Services ID → Sign in with Apple → Configure → Primary
  App ID), so the browser and the app see the same Apple subject.
- **The build carries the entitlement.** Tauri has no iOS entitlements
  setting and writes an empty file on every `tauri ios init`, so
  `Entitlements.ios.plist` beside `tauri.conf.json` is copied over it by
  `scripts/package-ios.sh`. For the dev loop, copy it once yourself after
  `tauri ios init`:

  ```sh
  cp src-tauri/Entitlements.ios.plist src-tauri/gen/apple/axon_iOS/axon_iOS.entitlements
  ```

Signing is automatic, so no profile is made by hand. Xcode regenerates the team
provisioning profile to match the entitlement, as long as it can reach the
account: an Apple ID in Xcode → Settings → Accounts, or an App Store Connect
API key exported as `APPLE_API_KEY`, `APPLE_API_ISSUER` and
`APPLE_API_KEY_PATH`, which the Tauri CLI passes to `xcodebuild`. A profile made
before the capability was enabled fails the build with "doesn't include the Sign
In with Apple capability" until it is regenerated.

An entitlement that is missing from the signed app does not fail the build: the
sheet never appears and Apple reports error 1000, which the app surfaces as a
pointer back here. `package-ios.sh` therefore checks the exported `.ipa` and
refuses to install or upload a build that lacks the entitlement, or the
`org.matrixaxon.axon` URL scheme that browser sign-in returns through.

### The signing team is not yours

`bundle.iOS.developmentTeam` in `tauri.conf.json` is one developer's Apple
team. Export your own before building:

```sh
export APPLE_DEVELOPMENT_TEAM=<Apple Developer > Membership > Team ID>
```

The CLI reads that variable and it overrides the config value. Since the
script regenerates `gen/apple` on every run, patching the generated Xcode
project instead does not survive.
[#456](https://github.com/matrix-axon/matrix-axon/issues/456) tracks taking
the committed default out.

### Signing without a password prompt

Signing happens inside `xcodebuild`, which uses the login keychain. That
keychain is locked in an SSH session and after a reboot, so an unattended
`scripts/package-ios.sh` stops at a password dialog, or fails with:

```
errSecInternalComponent
```

That error reads like a bad certificate. It is a locked keychain with nobody
there to unlock it.

Skip this section if you build at your own desk with the login keychain
unlocked. It is for a build box, or for anyone tired of the prompt.

Put the signing identities in a keychain of their own with an empty password,
and point the script at it:

```sh
security create-keychain -p "" build
security set-keychain-settings ~/Library/Keychains/build.keychain-db   # never auto-lock
security list-keychains -d user -s \
  ~/Library/Keychains/login.keychain-db ~/Library/Keychains/build.keychain-db

# Export each identity from Keychain Access as a .p12 (needs your login
# password once), then import it from the command line:
security import dev.p12 -k ~/Library/Keychains/build.keychain-db -P '<p12 password>' \
  -T /usr/bin/codesign -T /usr/bin/security
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "" \
  ~/Library/Keychains/build.keychain-db

export AXON_IOS_KEYCHAIN=build   # a name under ~/Library/Keychains, or a path
```

The script then unlocks it before the build. Which identities it needs depends
on `--export-method`:

| `--export-method`     | Identity             | Verified from the keychain alone |
| --------------------- | -------------------- | -------------------------------- |
| `debugging` (default) | `Apple Development`  | yes                              |
| `app-store-connect`   | `Apple Distribution` | yes                              |
| `release-testing`     | `Apple Distribution` | no — fails at export, see below  |

`release-testing` builds and archives, then fails at export with
`exportArchive No Accounts` and `No profiles for 'org.matrixaxon.axon' were
found`. That is a provisioning-profile problem, not a keychain one: the same
keychain completes `app-store-connect`. It is not yet diagnosed;
[#529](https://github.com/matrix-axon/matrix-axon/issues/529) tracks it.

Three things that are easy to get wrong:

- **Remove the identities from the login keychain afterwards.** An identity that
  exists in both keychains is resolved to the login copy, which is locked, even
  when `build` is listed first. The same certificate signed with the login copy
  gone and failed with it present. Delete the certificate and its private key
  together. The script does not yet warn about a leftover copy
  ([#528](https://github.com/matrix-axon/matrix-axon/issues/528)), so you find
  out at the signing step.
- **`set-key-partition-list` is not optional.** Without it macOS asks, through a
  dialog, whether `codesign` may use the key. Run it again after every import.
- **An empty password is the point, not an oversight.** Anyone who can read the
  keychain file can sign as you, which is why this belongs on a machine you
  already trust with the login keychain. If yours has a password, the script
  reads `AXON_IOS_KEYCHAIN_PASSWORD`; that puts it on a command line where `ps`
  shows it.

`--upload` authenticates through an App Store Connect API key, not the
keychain, so none of this applies to it.

To check the setup without a full build, sign a scratch file with the keychain
alone in the search list. With the login keychain also listed, a copy of the
identity left there decides the result, which is the case this test exists to
catch, so narrow the list first and put it back afterwards:

```sh
security list-keychains -d user          # note what is listed; you restore it below
security list-keychains -d user -s ~/Library/Keychains/build.keychain-db

t=$(mktemp) && cp /bin/echo "$t"
codesign -f -s "Apple Development: NAME (TEAMID)" "$t"; echo "exit $?"; rm -f "$t"

# Restore every keychain the first command printed, in that order:
security list-keychains -d user -s ~/Library/Keychains/login.keychain-db \
  ~/Library/Keychains/build.keychain-db
```

### Building from the Xcode GUI

Use `scripts/package-ios.sh` to build. Opening `gen/apple` in Xcode and pressing
Build fails, for reasons that are worth knowing if you do want Xcode, to attach
a debugger, say.

**Nothing is answering the build phase.** The project's "Build Rust Code" phase
runs `pnpm tauri ios xcode-script`, which does not know its own options. It asks
the running `tauri ios dev` or `tauri ios build` over a local socket, and finds
the address in `$TMPDIR/org.matrixaxon.axon-server-addr`. With no such command
running that file is left over from the last one, and the build dies:

```
thread '<unnamed>' panicked at crates/tauri-cli/src/mobile/mod.rs:403:6:
failed to read CLI options: ... Connection refused
```

**The environment is whichever process started the build.** Xcode opened from
the Dock, Finder or Spotlight is started by launchd, which has none of your
shell's setup: no `~/.cargo/bin`, no nvm node. (We saw pnpm fail to switch to
the version `package.json` pins, `ERR_PNPM_PNPM_ENGINE_NO_NATIVE_BINARY`; not
diagnosed further.) A build that `pnpm tauri ios dev` starts inherits your shell
instead, including a Homebrew `rust` ahead of rustup, and fails with

```
error[E0463]: can't find crate for `std`
  = note: the `aarch64-apple-ios` target may not be installed
```

straight after `component rust-std for target aarch64-apple-ios is up to date`.
That is the shadowing `package-ios.sh` guards against; put `~/.cargo/bin` first
in `PATH` before starting the command.

**What you change in Xcode does not last.** The script deletes and regenerates
`gen/apple` on every run.

If you want Xcode anyway, start `pnpm tauri ios dev --open` from a shell whose
`PATH` is right and leave it running while you build; it is the command that
answers the phase. We have not built a release that way.

To see why a build failed, read Xcode's log rather than the on-screen summary:
`~/Library/Developer/Xcode/DerivedData/axon-*/Logs/Build/*.xcactivitylog`, gzip.
A build started by `tauri ios dev` lands in a different `axon-*` folder from one
started in the GUI.

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
it. On iOS, Sign in with Apple uses the same `axon-desktop` registration and
additionally needs native Apple enabled on the server, with the app's bundle ID
as its audience (`docs/apple-oauth-native.md`):

```toml
[oauth.providers.apple]
native_enabled = true
native_audiences = ["org.matrixaxon.axon"]
```

The shell identifies itself as `axon-desktop` with the callback
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

## macOS signing and notarization

Unsigned macOS builds are refused by Gatekeeper with a malware warning, so the
lane is written to sign and notarize the moment the credentials exist — see
`.github/workflows/desktop-build.yml`, which reads them from repository secrets
and skips signing when they are absent. No workflow change is needed to turn
this on; only the secrets.

What does need to exist first, and is why this section is here rather than in a
future PR, is `Entitlements.plist`. Notarization requires the hardened runtime,
`bundle.macOS.hardenedRuntime` defaults to `true`, and the hardened runtime
denies the camera to an app that has not declared it. Adding the certificate
without the entitlement would therefore _lose QR sign-in on macOS_ — silently,
on a signed artifact, with the same wording that `NSCameraUsageDescription`
exists to fix.

`NSCameraUsageDescription` (in `Info.plist`) and the entitlement are not
alternatives. The description is what the system shows the user when it asks;
the entitlement is what permits the app to ask. Both are required, and they
live in different files.

The six secrets the macOS job reads:

| Secret                       | What it is                                                                     |
| ---------------------------- | ------------------------------------------------------------------------------ |
| `APPLE_CERTIFICATE`          | the **Developer ID Application** `.p12`, base64-encoded                        |
| `APPLE_CERTIFICATE_PASSWORD` | the password set when exporting that `.p12`                                    |
| `APPLE_SIGNING_IDENTITY`     | `Developer ID Application: NAME (TEAMID)`, exactly as the certificate names it |
| `APPLE_ID`                   | the Apple ID the Developer Program membership is under                         |
| `APPLE_PASSWORD`             | an **app-specific password**, not the Apple ID password                        |
| `APPLE_TEAM_ID`              | the 10-character Team ID                                                       |

The first three sign; the last three notarize. Notarization is a separate step
that uploads the signed bundle to Apple and staples the result, so all six are
needed for an artifact that opens without a warning.

**Developer ID Application, not Mac App Store.** Those are different
certificate types: Developer ID is for distributing outside the store, which is
what this lane builds. A store build (M-W13) is sandboxed, and the sandbox is a
different mechanism from the hardened runtime — `Entitlements.plist` says which
keys are deliberately absent for that reason, and would need revisiting rather
than extending.

Windows Authenticode is **not** wired yet, so SmartScreen warnings are a
separate piece of work.
