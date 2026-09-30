import { useEffect, useState } from 'preact/hooks'
import { App } from './app'
import { browserPlatform, type Platform } from './platform'
import { resolveApiBaseUrl } from './services'
import type { AppServices } from './services'
import { SHOW_PRIVACY_EVENT } from './pages/PrivacyPage'
import { ServerSetup } from './ServerSetup'
import { SHOW_HELP_EVENT } from './shortcuts'

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
      window.dispatchEvent(
        new Event(command === 'help' ? SHOW_HELP_EVENT : SHOW_PRIVACY_EVENT),
      )
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
