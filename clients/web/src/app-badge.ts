import { effect } from '@preact/signals'
import type { IconBadgeSetter } from './platform'
import type { RoomsStore } from './stores/rooms'
import type { SettingsStore } from './stores/settings'

/**
 * Whether this browser supports the Badging API (ADR 0080).
 *
 * Tests the properties with `typeof` rather than `in`: at least one WebKit
 * runtime declares `setAppBadge` on `Navigator` while leaving the accessor
 * `undefined`, so `in` answers `true` and the call then throws (#435). Both
 * halves are checked independently — a declaration that lies about one is no
 * evidence about the other, and `applyAppBadge` needs to call both.
 */
export function appBadgeAvailable(): boolean {
  return (
    typeof navigator.setAppBadge === 'function' &&
    typeof navigator.clearAppBadge === 'function'
  )
}

/**
 * Whether this engine is a build of Safari/WebKit where `setAppBadge` resolves
 * successfully but silently renders nothing unless Notification permission has
 * been granted — confirmed on iOS 16.4+ and iOS 27 beta (ADR 0080). There is
 * no feature-detectable signal for this: the call's promise behaves
 * identically either way, so browser sniffing is the only lever. Deliberately
 * excludes Chromium/Firefox-on-iOS UAs (`CriOS`/`FxiOS`/`EdgiOS`/`OPiOS`),
 * which embed WebKit under Apple's App Store rules but aren't the confirmed
 * case, and excludes Android to be safe against any UA that happens to
 * include the token `Safari` there too.
 */
export function badgeNeedsNotificationPermission(): boolean {
  const ua = navigator.userAgent
  return (
    /Safari/.test(ua) && !/Chrome|CriOS|FxiOS|EdgiOS|OPiOS|Android/.test(ua)
  )
}

/** Whether the Notification API exists at all in this context. */
export function notificationPermissionAvailable(): boolean {
  return typeof Notification !== 'undefined'
}

/**
 * Ask for Notification permission purely to unlock Safari's badge-rendering
 * gate. The grant does not turn on message notifications; that is a separate
 * Settings choice. Returns `null`
 * without prompting when the permission is already decided (`granted` or
 * `denied`, which JS cannot re-prompt for) or the API doesn't exist.
 *
 * Must be called synchronously from within a real user-gesture event handler
 * (a click/tap): Safari rejects `Notification.requestPermission()` calls made
 * any other way — including from a promise continuation, `setTimeout`, or a
 * reactive effect — with "Notification prompting can only be done from a user
 * gesture." This is why the badge setting's own checkbox can't reliably drive
 * this: the setting defaults to on (ADR 0080), so most users never click it.
 */
export function requestAppBadgeNotificationPermission(): Promise<NotificationPermission> | null {
  if (
    !notificationPermissionAvailable() ||
    Notification.permission !== 'default'
  ) {
    return null
  }
  return Notification.requestPermission()
}

/**
 * The Badging API, or `null` when this context cannot call it.
 *
 * `null` is only the browser path's "absent" answer. A shell passes its own
 * setter and never reaches this.
 */
/**
 * Read the inputs the effect has to follow.
 *
 * A bare property read is an unused expression, and the flush below reads
 * the signals again so a burst keeps only its last value. Calling this is
 * what subscribes the effect.
 */
function watchBadgeInputs(
  unread: number,
  enabled: boolean,
  loading: boolean,
): void {
  if (
    typeof unread === 'number' &&
    typeof enabled === 'boolean' &&
    typeof loading === 'boolean'
  ) {
    return
  }
}

function navigatorBadge(): IconBadgeSetter | null {
  if (!appBadgeAvailable()) {
    return null
  }
  return (count) =>
    count === null ? navigator.clearAppBadge() : navigator.setAppBadge(count)
}

/**
 * Reflect the unread-rooms count onto the app icon while
 * `settings.appBadgeEnabled` is on. Mirrors `unreadTotal`, the same total
 * `RoomList` already shows — one definition of "unread" everywhere it's
 * counted.
 *
 * `setIconBadge` is the shell. Pass `null` (the default) in a browser, which
 * then uses the Badging API (ADR 0080). The packaged webviews either omit
 * that API or resolve it without painting, so a shell that has its own
 * setter must not also call `navigator`.
 *
 * One effect covers both. A room-list refresh writes `unreadTotal` once per
 * changed room, and this used to invoke on every write, including the
 * initial 0 from before the list has loaded. The flush waits until `loading`
 * has been false once — that first 0 is "not loaded", not "nothing unread" —
 * and sends only the last value of the turn, so a burst cannot paint an
 * intermediate total.
 */
export function applyAppBadge(
  settings: SettingsStore,
  rooms: RoomsStore,
  setIconBadge: IconBadgeSetter | null = null,
): () => void {
  const paint = setIconBadge ?? navigatorBadge()
  if (paint === null) {
    // Distinguishes "the API is genuinely absent here" from "it's present but
    // silently doing nothing" — indistinguishable from the outside otherwise,
    // and the difference matters most on iOS, where WebKit only exposes
    // `setAppBadge` on `navigator` once the page is running as an installed,
    // standalone home-screen web app. "Unavailable" here also covers the
    // declared-but-`undefined` case that `appBadgeAvailable` now screens for.
    console.info(
      'app-badge: navigator.setAppBadge/clearAppBadge is unavailable in this context',
    )
    return () => {}
  }

  let disposed = false
  let scheduled = false
  // `undefined` means nothing has been sent yet, which is not the same as a
  // clear. The first settled 0 still has to be delivered.
  let lastSent: number | null | undefined
  let sawSettled = false
  const stop = effect(() => {
    // Read during the effect so a later write reschedules. The flush reads
    // them again and keeps only the last value of this turn.
    watchBadgeInputs(
      rooms.unreadTotal.value,
      settings.appBadgeEnabled.value,
      rooms.loading.value,
    )
    if (scheduled) {
      return
    }
    scheduled = true
    queueMicrotask(() => {
      scheduled = false
      if (disposed) {
        return
      }
      // The store starts at 0 with `loading` still true. Sending that would
      // clear the icon before the list exists, and on iOS a badge of 0 also
      // removes delivered notifications. A later clear, including sign-out,
      // still sends: by then the total has been settled once.
      if (!sawSettled) {
        if (rooms.loading.value) {
          return
        }
        sawSettled = true
      }
      const count = rooms.unreadTotal.value
      const shown = settings.appBadgeEnabled.value && count > 0 ? count : null
      if (shown === lastSent) {
        return
      }
      lastSent = shown
      try {
        const call = paint(shown)
        // A rejection is environmental, not the Notification-permission gate
        // (Safari resolves either way per ADR 0080). Routed through
        // `Promise.resolve` because an implementation that returns nothing
        // would make a bare `.catch` throw.
        void Promise.resolve(call).catch((cause: unknown) => {
          console.error('app-badge: badge call failed', cause)
        })
      } catch (cause) {
        // Nothing thrown by a decorative badge is worth propagating. This
        // effect re-runs on every unread-count change, so a throw escapes
        // into whatever wrote `unreadTotal` — the sync path (#435).
        console.error('app-badge: badge call threw', cause)
      }
    })
  })
  return () => {
    disposed = true
    stop()
  }
}
