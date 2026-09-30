# ADR 0107 — Platform-standard keyboard shortcuts in the native shell

## Context

ADR 0078 chose the web client's chords to get around the browser, not to follow
the platform. A page cannot have `Ctrl-N` (new window). Chrome shows its find
bar on `Ctrl-F`, and `Ctrl-,`/`⌘-,` and `F1` open the browser's own settings
and help. So search became `Ctrl-Shift-F`, or `⌘-G` on macOS (ADR 0066), and
starting a DM became `Ctrl-Alt-M`. Help became `?` or `Ctrl-/`, and settings
got no chord at all.

The Tauri shell (ADR 0102) runs the same `dist` with no browser chrome around
it, so none of those reservations apply. Users there expect a native app's
keys, and `⌘-F` that does nothing looks like a bug.

## Decision

Inside the shell (`isTauriRuntime()`), the web client also binds the platform's
standard chord for every action that ADR 0078 had to move:

| Action          | Browser                     | Shell, Windows/Linux | Shell, macOS/iPadOS |
| --------------- | --------------------------- | -------------------- | ------------------- |
| Search messages | `/`, `Ctrl-Shift-F` / `⌘-G` | `/`, `Ctrl-F`        | `/`, `⌘-F`          |
| Start a DM      | `Ctrl-Alt-M` / `⌘-Option-M` | `Ctrl-N`             | `⌘-N`               |
| Open settings   | —                           | `Ctrl-,`             | `⌘-,`               |
| Show help       | `?`, `Ctrl-/` / `⌘-/`       | `?`, `F1`            | `?`, `⌘-?`          |

- **The browser chords stay bound in the shell**, so habits formed in the web
  client still work. The one exception is `⌘-G`, which means Find Next in a
  native Mac app, so the shell stops treating it as Find.
- **Nothing changes in a browser.** The new chords are gated on the runtime,
  so a browser still gets its own find bar, new window and settings.
- **The platform chords check the modifier they actually mean.** `chordOf`
  folds Ctrl and ⌘ into `mod`, but on macOS `Ctrl-F`/`Ctrl-N` are the Emacs
  cursor keys every text field honours, and `Ctrl-⌘-F` is the menu's
  full-screen toggle. `isPrimaryModifier` requires ⌘ alone on Apple platforms
  and Ctrl alone elsewhere, so those keys still reach the composer and the
  menu.
- **Labels follow the runtime.** A `KEYS` entry can carry a `native` override,
  and `keyLabel`/`keyAria` choose it inside the shell. The help popup,
  tooltips and `aria-keyshortcuts` therefore advertise the chord that is really
  bound, which keeps ADR 0078's anti-drift rule. A row that exists only in the
  shell (settings) is marked `nativeOnly`, and `shortcutGroups()` leaves it out
  of a browser's help.

## Consequences

- The shell and the browser advertise different chords for the same action.
  This is deliberate: each one lists what works where it runs.
- Room and space stepping, filter and sort cycling, the sidebar toggle and the
  composer-height chords are unchanged. No platform convention exists for any
  of them, or the browser never forced them off one.
- WebView2 uses `Ctrl-F` for its own find bar unless the page cancels the
  keydown. The search handler calls `preventDefault()`, and a Windows build
  confirms that the app's search opens, not WebView2's find bar.

## Page zoom

A browser zooms on `Ctrl-+`/`Ctrl--`/`Ctrl-0` without any help from the
page. The shell's webview does not, so the desktop shell binds those chords
itself (`⌘` on macOS). It steps through the browsers' own zoom levels, from
50% to 300%, and stores the level in settings, so it survives a restart.

- **Tauri's `zoomHotkeysEnabled` is not used.** On macOS and Linux it injects
  a script that also zooms 20% for every Ctrl-wheel event. A trackpad pinch
  arrives as a burst of exactly those events, so a single pinch would jump the
  window to its 20% or 1000% limit. The script also starts from 100% on every
  launch. The client calls `getCurrentWebview().setZoom()` through a new
  `Platform.setZoom` instead, which needs the
  `core:webview:allow-set-webview-zoom` capability.
- **Desktop only.** `setZoom` is `null` in a browser, which keeps its own
  per-site zoom, and in the iOS/Android shells, where pinch is the zoom and
  Tauri documents page zoom as unsupported. Where it is `null`, the chords are
  left alone and the help leaves out the zoom rows.
- **Ctrl-scroll does not zoom.** The desktop shell has keyboard zoom only.
  Wheel zoom would need a handler that can tell a mouse notch from a stream of
  pinch events, and it can be added later if people miss it.

### The image viewer zooms its own image

Page zoom does not enlarge a photo in the viewer, which caps the image at the
viewport, and on a phone a pinch in the viewer had nothing to act on. So the
viewer has its own zoom (`media/image-zoom.ts`), in the browser and the shell
alike:

- **Pinch, or Ctrl/⌘-scroll, zooms where you point.** Ctrl-scroll is also how
  Chromium and Firefox report a trackpad pinch. WebKit reports one as
  `gesture*` events, which the viewer handles too, for Safari and the macOS
  shell.
- **A zoomed image pans** with one finger, a mouse drag or a plain scroll.
  While zoomed, swipe paging and swipe-down-to-dismiss are off, and a drag
  does not also toggle the immersive mode.
- **Keys and buttons.** `+`/`-`/`0` work, and so do the Ctrl/⌘ chords. The
  viewer claims those before the shell's page zoom sees them. Zoom-in and
  zoom-out buttons sit in the toolbar. Escape returns a zoomed image to fit
  before it closes the viewer. Paging to the next image starts at fit.
- **Images only.** The image registers itself with the viewer through a
  context, so a video or a PDF gets no zoom controls.

## Help menu and privacy policy

App Store review requires the privacy policy to be reachable from inside the
app, and a macOS user expects the app's help in the Help menu.

- **The policy is bundled, not linked.** `docs/PRIVACY_POLICY.md` stays the one
  copy, and the README and store listings link to it on GitHub. A Vite plugin
  (`axon-privacy-policy`) renders it to HTML at build time, and the build fails
  if the file is missing, so no build can ship without it.
  `deploy/web/Dockerfile` copies the file in, and `.dockerignore` makes an
  exception for it.
- **Reachable from every screen, on every platform.** Signed in, it is the
  `/privacy` route, linked from the Settings footer next to the open-source
  licenses and from the help dialog. The server-setup and sign-in screens come
  before the router, so they show it in place with a Back button. An App Store
  reviewer meets those screens first.
- **macOS gets a Help menu.** The shell keeps Tauri's default menu bar and adds
  "Axon Help" (⇧⌘/, which is ⌘?) and "Privacy Policy" to its Help menu. The
  menu only relays: it emits `axon://menu` and `AppRoot` turns that into the
  page's own events, so every screen answers with what it already has.
- **No menu bar on Windows or Linux.** Tauri gives them none. Adding one just
  for Help would look out of place in a chat app, and Slack, Discord and
  Element on those platforms keep these links inside the app. The Settings
  footer and help dialog links are the Windows and Linux route, and they are
  also where the iOS build's reviewers will find the policy.

## macOS text services

Autocorrect did nothing in the macOS shell, although it works in Safari. The
cause is one WebKit default, not the menu bar. WebKit's `TextCheckerMac.mm`
takes autocorrect, smart quotes, smart dashes and text replacement from the
system settings unless the app has its own `Web…Enabled` default. It takes
spell checking while typing only from the app's `WebContinuousSpellCheckingEnabled`,
which nothing had set, so it was off. Autocorrect runs as part of spell
checking, so it was off with it.

- **The shell registers `WebContinuousSpellCheckingEnabled = YES` at launch**,
  before the webview exists. It uses `registerDefaults`, so the value sits in
  the registration domain, below the app's own defaults, and the user's
  choice wins once they make one.
- **The Edit menu gains Spelling and Grammar and Substitutions**, with the
  same items as Safari. Tauri's menu library has no such items, so they are
  AppKit items with no target. The action goes to the focused webview, and
  WKWebView implements each one, shows its checkmark state and saves a toggle
  as the app's default. The menu lets users change these settings; the
  defaults above are what make them work out of the box.
