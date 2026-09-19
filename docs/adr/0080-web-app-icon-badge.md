# ADR 0080 — Web app icon unread badge

## Context

The web client can be installed as a PWA (`clients/web/public/manifest.webmanifest`,
`display: "standalone"`), and Settings already has an "Install app" section
(`InstallAppSettings` in `clients/web/src/pages/SettingsPage.tsx`) that detects
platform and install state. Once installed, the app icon on a taskbar, dock, or
home screen carries no indication of unread activity — the user has to switch
to the app to find out.

The Badging API (`navigator.setAppBadge(count)` / `navigator.clearAppBadge()`)
lets an installed PWA set a small numeric badge on its icon, supported in
Chromium-based desktop and Android, and Safari on iOS/iPadOS/macOS. Per spec it
is distinct from the Notification/Push API — no permission prompt required, no
service worker, no server-side push infrastructure, all of which are out of
scope per ADR 0031 and ADR 0053. This client already has none of those (no
service worker exists in the repo today), so this feature stays a local,
session-derived reflection of state already in memory — it does nothing while
the tab/app is closed.

In practice, on Safari/WebKit that "no permission" claim is only half true.
`setAppBadge`'s promise resolves successfully with no error either way, but
WebKit silently renders nothing unless the page has been granted Notification
permission — confirmed by testing (see Consequences): a bare
`navigator.setAppBadge(1)` on a throwaway page shows nothing until
`Notification.requestPermission()` is granted, after which the exact same call
displays the badge. This is presumably WebKit reusing the same OS-level
authorization plumbing native iOS apps have always used to gate badges
(`Settings → Notifications → App → Badges`), rather than building a separate
consent model for the web version. There is no way to detect this
programmatically — the call's outcome looks identical whether it's silently
suppressed or genuinely unsupported — so working around it means asking for
Notification permission anyway, purely to unlock badge rendering, never to
show an actual notification.

Per-room unread counts are already tracked in `RoomsStore` (`clients/web/src/
stores/rooms.ts`): `unreadKeys` is the set of room keys with a nonzero
server-derived `notification_count`, and `RoomList.tsx` surfaces
`unreadKeys.size` as a rooms-unread total. An early version of this ADR
reused that room-count total for the app icon badge, on the theory that it
matched `ThreadUnreadStore.count`'s existing "count of items, not summed
messages" convention. In testing, that reads as broken: once one room goes
unread the badge shows 1 and *stays* 1 while further messages pile up in that
same room, since a room already in `unreadKeys` doesn't get added again. An
app icon badge is conventionally a message/mention volume (Slack, Discord,
iOS Mail), not a distinct-conversation count, so the badge instead sums each
room's `notificationCount` — a new `RoomsStore.unreadTotal` signal, maintained
incrementally (see Decision) rather than reusing `unreadKeys`. This is a
deliberate divergence from the room list's own per-room badges, which stay a
room-nonzero-count concept; the app icon is the one place actual message
volume is what a badge conventionally means.

## Decision

- **New setting `appBadgeEnabled: boolean`** (default **`true`**) in the
  schema-versioned `SettingsV1` envelope (`stores/settings.ts`), following the
  existing boolean-preference pattern (`developerMode`, `perfMarks`, etc.). An
  earlier draft of this ADR defaulted it off, on the `matrixProtocolHandler`
  theory that an OS-level icon change is a bigger commitment than an in-page
  preference. Two things overturned that:
  - Testing on iOS surfaced that a Home Screen web app gets **its own storage,
    separate from the Safari tab it was added from**. A setting toggled on
    beforehand in the tab has no way to reach the fresh standalone instance —
    there is no discovery path from "off by default" to "on" that doesn't
    require digging into Settings a second time after every install, with
    nothing prompting the user to do so.
  - Unlike `matrixProtocolHandler` (a real browser permission prompt) or
    Notifications (a permission the user must be asked to grant), the Badging
    API needs no permission and touches nothing off-device — there is no
    OS-level consent being front-run by defaulting it on. Messaging apps
    generally badge without asking (Slack, WhatsApp, Mail); opt-out, not
    opt-in, matches that expectation.
- **New module `clients/web/src/app-badge.ts`** owns the Badging API bridge,
  mirroring the `install-prompt.ts` shape (a capability-check function plus a
  setup function returning a disposer, no signals needed here since it has no
  independent browser-driven state to observe):
  - `appBadgeAvailable(): boolean` — `typeof` checks that both
    `navigator.setAppBadge` and `navigator.clearAppBadge` are functions (a bare
    `in` test accepts a runtime that declares the key but leaves it
    `undefined`; see #435). On iOS this is only `true` once the page is already running as an installed,
    standalone home-screen web app — WebKit does not expose the method at all
    in an ordinary Safari tab.
  - `applyAppBadge(settings: SettingsStore, rooms: RoomsStore): () => void` —
    an `effect()` that calls `navigator.setAppBadge(rooms.unreadTotal.value)`
    when the setting is on and the total is nonzero, `navigator.clearAppBadge()`
    otherwise. Returns the effect's disposer, wired at app root exactly like
    `applyTheme`: `useEffect(() => applyAppBadge(svc.settings, svc.rooms),
    [svc])` in `app.tsx`. Logs to the console (not just on a rejected badge
    call, but also when the API is absent) — the two failure modes are
    otherwise indistinguishable from outside the module, and on iOS that
    distinction is exactly what tells you whether you're looking at "not
    installed yet" versus something actually broken.
  - Not gated on install/standalone state in the checkbox itself: `disabled`
    was tried and reverted — since `appBadgeAvailable()` is `false` pre-install
    on iOS, disabling on it made the setting untogglable until *after* install,
    which is backwards (the user needs to be able to opt in before or
    independent of installing). The checkbox is always interactive; an
    unsupported-here note explains why nothing visible happens yet, and the
    preference is saved and takes effect the moment the environment supports
    it (e.g. after installing to the home screen and reopening from there).
- **New checkbox in `InstallAppSettings`** (`SettingsPage.tsx`). Copy: "Show
  unread count on the app icon", with a note shown only when
  `!appBadgeAvailable()` explaining the install requirement rather than
  implying the browser lacks the feature outright.
- **New `RoomsStore.unreadTotal` signal** (`stores/rooms.ts`), maintained
  alongside `unreadKeys` inside `setUnreadCounts`: each call that changes a
  room's `notificationCount` adds the delta (`next - previous`) to a running
  total, rather than re-summing every room on every update. This keeps the
  room-list-perf invariant (no new signal writes scale with the number of
  rooms on a hot per-message path) — one number add per live count update,
  same as the existing `unreadKeys` set maintenance beside it.
- **`app-badge.ts` also exports a Notification-permission unlock, kept
  separate from `applyAppBadge`:**
  - `badgeNeedsNotificationPermission(): boolean` — UA-sniffs for real
    Safari (`Safari` present, `Chrome`/`CriOS`/`FxiOS`/`EdgiOS`/`OPiOS`/
    `Android` absent). There is no feature-detectable signal for "this engine
    gates badge rendering on notification permission," so sniffing is the
    only lever; deliberately excludes Chromium (confirmed working on Windows
    with no permission ever requested — asking there would be a pointless,
    unrelated prompt) and iOS browsers riding WebKit under Apple's App Store
    rules but not confirmed to share this behavior.
  - `requestAppBadgeNotificationPermission(): Promise<NotificationPermission>
    | null` — calls `Notification.requestPermission()`, returning `null`
    without prompting when the permission is already decided (`granted` or
    `denied`, neither of which JS can re-prompt for) or the API doesn't exist.
  - **Not wired to the badge checkbox's `onChange`.** Safari only honors
    `Notification.requestPermission()` from inside a real user-gesture
    handler, and since `appBadgeEnabled` now defaults to `true`, most users
    never click that checkbox at all — there'd be no gesture to hang the
    request on. Instead, `InstallAppSettings` shows a dedicated "Allow
    notifications to enable the badge" button whenever
    `badgeNeedsNotificationPermission()` is true and permission is still
    `'default'`, independent of the checkbox's state, plus a static note if
    permission comes back `'denied'` (JS cannot re-prompt from there; the user
    has to go through system settings).

## Consequences

- Clearing the badge on sign-out or when the tab loses all unread rooms is
  automatic: the effect re-runs on every `unreadKeys` change, including down to
  zero, and unmounting the app (`app.tsx` teardown) is not a case this needs to
  handle explicitly — `clearAppBadge` on next launch with zero unread rooms is
  sufficient; a badge left stuck from an abrupt tab close is a known limitation
  of the API itself, not something this client can prevent.
- No test coverage for the actual OS-level badge rendering (no jsdom
  `navigator.setAppBadge`); tests cover `applyAppBadge`'s call pattern against
  a mocked `navigator`, matching how `install-prompt.ts` is tested today.
- If push notifications are added later (currently out of scope), this same
  effect is the natural place to also badge from a service-worker-delivered
  count while the tab is closed — not addressed here.
- **Root-caused on iOS 27 Developer Beta**: `setAppBadge` resolved
  successfully with the right count, but the home-screen icon showed nothing —
  reproduced even with a bare `navigator.setAppBadge(1)` on a throwaway page,
  ruling out anything app-specific. Initially suspected as a beta-only WebKit
  regression; actually resolved by granting Notification permission (via a
  gesture-triggered `Notification.requestPermission()` in the console) after
  which the identical `setAppBadge` call displayed the badge immediately. Not
  beta-specific as far as this investigation could tell — untested on a
  stable iOS release, but nothing about the mechanism looks beta-only.
- The permission-request UI can only be validated manually (no jsdom
  `Notification` implementation, no way to simulate the OS permission sheet);
  `app-badge.test.ts` covers `badgeNeedsNotificationPermission`'s UA sniffing
  and `requestAppBadgeNotificationPermission`'s decided-vs-`'default'`
  branching against a mocked `Notification` global, not the actual dialog or
  its effect on rendering.
