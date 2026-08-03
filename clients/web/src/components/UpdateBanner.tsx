import { useState } from 'preact/hooks'
import { formatVersion } from '../build-info'
import { browserReloadEnvironment, reloadNow } from '../reload'
import { useServices } from '../services'

/**
 * The interactive half of automatic refresh (ADR 0087): what the user sees when
 * a new build is available and reloading is *not* free — they are looking at
 * the app, or they have something unsent. `startAutoRefresh` handles the case
 * where it is free by reloading silently, so reaching this bar means the choice
 * genuinely belongs to the user.
 *
 * Dismissal is per-visit and deliberately not persisted. The update does not go
 * away, and the next time they leave the app idle it will apply itself; a
 * dismissal that outlived the session would turn "not now" into "never".
 */
export function UpdateBanner() {
  const { updates } = useServices()
  const [dismissed, setDismissed] = useState(false)

  if (!updates.available.value || dismissed) {
    return null
  }

  const latest = updates.latest.value
  const label =
    latest === null ? null : formatVersion(latest.release, latest.version)

  return (
    <div class="banner notice info shell-banner update-banner" role="status">
      <span>
        A new version of Axon is available
        {label === null ? null : (
          <>
            {' '}
            · <code>{label}</code>
          </>
        )}
      </span>
      <span class="update-banner-actions">
        <button
          type="button"
          onClick={() => reloadNow(browserReloadEnvironment())}
        >
          Reload
        </button>
        <button
          type="button"
          class="ghost"
          aria-label="Dismiss update notice"
          onClick={() => setDismissed(true)}
        >
          ×
        </button>
      </span>
    </div>
  )
}
