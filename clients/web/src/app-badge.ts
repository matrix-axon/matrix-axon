import { effect } from '@preact/signals'
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
 * gate — Axon never shows a notification through this grant. Returns `null`
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
 * Reflect the unread-rooms count onto the app icon (ADR 0080) while
 * `settings.appBadgeEnabled` is on. Mirrors `unreadKeys.size`, the same total
 * `RoomList` already shows — one definition of "unread" everywhere it's
 * counted. A no-op where the API is unsupported.
 */
export function applyAppBadge(
  settings: SettingsStore,
  rooms: RoomsStore,
): () => void {
  if (!appBadgeAvailable()) {
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
  return effect(() => {
    const count = rooms.unreadTotal.value
    const wanted = settings.appBadgeEnabled.value && count > 0
    try {
      const call = wanted
        ? navigator.setAppBadge(count)
        : navigator.clearAppBadge()
      // A rejection here is not the Notification-permission gate (Safari
      // resolves either way per ADR 0080) but something environmental — worth
      // a console trace instead of disappearing silently, since there is no UI
      // surface for it. Routed through `Promise.resolve` because an
      // implementation that returns nothing would make a bare `.catch` throw.
      void Promise.resolve(call).catch((cause: unknown) => {
        console.error('app-badge: navigator badge call failed', cause)
      })
    } catch (cause) {
      // Nothing thrown by a decorative badge is worth propagating. This effect
      // re-runs on every unread-count change, so a throw escapes into whatever
      // wrote `unreadTotal` — the sync path (#435).
      console.error('app-badge: navigator badge call threw', cause)
    }
  })
}
