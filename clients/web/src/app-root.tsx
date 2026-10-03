import { useEffect, useState } from 'preact/hooks'
import { App } from './app'
import { browserPlatform, type Platform } from './platform'
import { resolveApiBaseUrl } from './services'
import type { AppServices } from './services'
import { SHOW_PRIVACY_EVENT } from './pages/PrivacyPage'
import { ServerSetup } from './ServerSetup'
import { isApplePlatform, SHOW_HELP_EVENT } from './shortcuts'
import type { MenuCommand } from './platform'

/**
 * The mount point: the server gate in front of the app (ADR 0102 § 3).
 *
 * `createServices()` builds the whole graph around one base URL, so the base
 * has to be known before `App` mounts — which is why this wrapper exists
 * rather than a branch inside `App`, whose hooks all run before it could
 * decide anything.
 *
 * In a browser this is inert: `resolveApiBaseUrl` falls back to the platform's
 * `'/'` and `App` renders on the first pass, exactly as it did when `main.tsx`
 * rendered `App` directly. Only a packaged build, which has no same-origin API
 * to assume, can see `null` here.
 *
 * An injected `services` skips the gate outright. Tests that supply their own
 * graph have already answered the question this screen asks, and making them
 * all click through it would be pure ceremony.
 */
/** The key each View-menu zoom item stands for. */
const ZOOM_KEYS: Record<Exclude<MenuCommand, 'help' | 'privacy'>, string> = {
  'zoom-in': '=',
  'zoom-out': '-',
  'zoom-reset': '0',
}

/**
 * Answer a View-menu zoom item by replaying its key press in the page.
 *
 * The menu is the fallback for a ⌘= that the webview did not hand the page,
 * so it should do exactly what that key press would have done, wherever it
 * would have done it: zoom the image while the viewer has one open, else the
 * page (ADR 0107). Replaying the chord gets that from the handlers that already
 * decide it, rather than a second routing table that could drift from them.
 * Sent from the focused element, as a real key press would be. A synthetic
 * keydown types no text and never reaches the OS, so it cannot loop back into
 * the menu.
 */
function replayZoomKey(command: keyof typeof ZOOM_KEYS): void {
  const apple = isApplePlatform()
  const target = document.activeElement ?? document.body
  target.dispatchEvent(
    new KeyboardEvent('keydown', {
      key: ZOOM_KEYS[command],
      metaKey: apple,
      ctrlKey: !apple,
      bubbles: true,
      cancelable: true,
    }),
  )
}

export function AppRoot({
  services,
  platform = browserPlatform(),
  storage = window.localStorage,
}: {
  services?: AppServices
  platform?: Platform
  storage?: Storage
}) {
  const [baseUrl, setBaseUrl] = useState(() =>
    resolveApiBaseUrl(storage, platform),
  )

  // The macOS Help menu (ADR 0107). Relayed as window events from here, above
  // the server gate, because the menu is live on every screen and each screen
  // answers for itself: the shell opens help or routes to `/privacy`, and the
  // screens before sign-in show the policy in place.
  useEffect(() => {
    const subscribe = platform.onMenuCommand
    if (subscribe === null) {
      return
    }
    return subscribe((command) => {
      if (command === 'help' || command === 'privacy') {
        window.dispatchEvent(
          new Event(command === 'help' ? SHOW_HELP_EVENT : SHOW_PRIVACY_EVENT),
        )
        return
      }
      replayZoomKey(command)
    })
  }, [platform])

  if (services === undefined && baseUrl === null) {
    return (
      <ServerSetup
        onConnected={setBaseUrl}
        platform={platform}
        storage={storage}
      />
    )
  }
  return <App services={services} platform={platform} storage={storage} />
}
