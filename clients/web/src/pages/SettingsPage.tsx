import { useState } from 'preact/hooks'
import {
  appBadgeAvailable,
  badgeNeedsNotificationPermission,
  notificationPermissionAvailable,
  requestAppBadgeNotificationPermission,
} from '../app-badge'
import { BUILD_INFO } from '../build-info'
import { CopyableText } from '../components/CopyableText'
import {
  installOutcome,
  installPromptAvailable,
  promptInstallApp,
} from '../install-prompt'
import {
  matrixProtocolHandlerAvailable,
  registerMatrixProtocolHandler,
} from '../matrix-protocol'
import { browserReloadEnvironment, reloadNow } from '../reload'
import { useServices } from '../services'
import { currentPlatform, isApplePlatform } from '../shortcuts'
import type {
  StateEventVisibility,
  Theme,
  TimeFormat,
} from '../stores/settings'
import { AccountLifecycle } from './AccountsPage'

const THEMES: { value: Theme; label: string }[] = [
  { value: 'system', label: 'System' },
  { value: 'light', label: 'Light' },
  { value: 'dark', label: 'Dark' },
]

const TIME_FORMATS: { value: TimeFormat; label: string }[] = [
  { value: '12h', label: '12-hour (3:05pm)' },
  { value: '24h', label: '24-hour (15:05)' },
]

const STATE_EVENTS: { value: StateEventVisibility; label: string }[] = [
  { value: 'hidden', label: 'Hidden' },
  { value: 'important', label: 'Membership and profile changes' },
  { value: 'all', label: 'All state events' },
]

/** Theme + (schema-versioned) local settings (ADR 0046, M-W3). */
export function SettingsPage() {
  const { auth, settings, rooms, deviceState } = useServices()
  const [markingRead, setMarkingRead] = useState(false)
  const [protocolMessage, setProtocolMessage] = useState<string | null>(null)

  const markAllRead = async () => {
    setMarkingRead(true)
    let current = rooms.rooms.value
    try {
      // Marking every room read off a *cached* list would silently skip rooms
      // joined since it was written, so this waits for a confirmed list rather
      // than settling for whatever is on screen.
      await rooms.ensureLoaded()
      current = rooms.rooms.value
      const accounts = new Set(current.map((room) => room.account_id))
      await Promise.all(
        [...accounts].map((accountId) =>
          deviceState.markRoomSummariesRead(accountId, current),
        ),
      )
    } catch {
      // The device-state store keeps the optimistic read-marker cache and
      // requeues network-failed writes; the command should still clear local
      // badges instead of throwing from a fire-and-forget click handler.
    } finally {
      for (const room of current) {
        rooms.noteUnreadCounts(room.account_id, room.room_id, 0, 0)
      }
      setMarkingRead(false)
    }
  }

  const setMatrixProtocolHandler = (enabled: boolean) => {
    if (!enabled) {
      settings.matrixProtocolHandler.value = false
      setProtocolMessage(null)
      return
    }
    const result = registerMatrixProtocolHandler()
    if (result.ok) {
      settings.matrixProtocolHandler.value = true
      setProtocolMessage('Matrix link handling registered for this browser.')
    } else {
      settings.matrixProtocolHandler.value = false
      setProtocolMessage(result.message)
    }
  }

  return (
    <div class="page">
      <h1>Settings</h1>
      <section class="panel">
        <h2>Theme</h2>
        <div class="theme-picker" role="radiogroup" aria-label="Theme">
          {THEMES.map(({ value, label }) => (
            <label key={value}>
              <input
                type="radio"
                name="theme"
                value={value}
                checked={settings.theme.value === value}
                onChange={() => (settings.theme.value = value)}
              />
              {label}
            </label>
          ))}
        </div>
      </section>
      <section class="panel">
        <h2>Messages</h2>
        <h3 class="settings-group-label" id="settings-state-events">
          State events
        </h3>
        <div
          class="theme-picker"
          role="radiogroup"
          aria-labelledby="settings-state-events"
        >
          {STATE_EVENTS.map(({ value, label }) => (
            <label key={value}>
              <input
                type="radio"
                name="state-events"
                value={value}
                checked={settings.stateEvents.value === value}
                onChange={() => (settings.stateEvents.value = value)}
              />
              {label}
            </label>
          ))}
        </div>
        <p class="muted">
          Membership and profile changes are joins, leaves, invites, kicks and
          display-name changes. All state events adds topic, name, power-level
          and other room-configuration changes.
        </p>
        <label class="setting-row">
          <input
            type="checkbox"
            checked={settings.hideRedactedEvents.value}
            onChange={(event) =>
              (settings.hideRedactedEvents.value = event.currentTarget.checked)
            }
          />
          Hide deleted messages
        </label>
        <p class="muted">
          Remove redacted (deleted) placeholders from messages.
        </p>
        <h3 class="settings-group-label" id="settings-time-format">
          Timestamp format
        </h3>
        <div
          class="theme-picker"
          role="radiogroup"
          aria-labelledby="settings-time-format"
        >
          {TIME_FORMATS.map(({ value, label }) => (
            <label key={value}>
              <input
                type="radio"
                name="time-format"
                value={value}
                checked={settings.timeFormat.value === value}
                onChange={() => (settings.timeFormat.value = value)}
              />
              {label}
            </label>
          ))}
        </div>
      </section>
      <section class="panel">
        <h2>Rooms</h2>
        <label class="setting-row">
          <input
            type="checkbox"
            checked={settings.previewRoom.value}
            onChange={(event) =>
              (settings.previewRoom.value = event.currentTarget.checked)
            }
          />
          Preview room
        </label>
        <p class="muted">Show the latest messages in the room list.</p>
        <label class="setting-row">
          <input
            type="checkbox"
            checked={settings.cacheRoomList.value}
            onChange={(event) =>
              (settings.cacheRoomList.value = event.currentTarget.checked)
            }
          />
          Keep the room list on this device
        </label>
        <p class="muted">
          Shows your rooms immediately rather than wait for the server, and
          keeps them visible when you're offline. Stores room names, topics and
          unread counts unencrypted on this device. Turning this off erases any
          stored data.
        </p>
        <button type="button" onClick={() => void markAllRead()}>
          {markingRead ? 'Marking…' : 'Mark all as read'}
        </button>
      </section>
      <InstallAppSettings />
      <section class="panel">
        <h2>Matrix links</h2>
        <label class="setting-row">
          <input
            type="checkbox"
            checked={settings.matrixProtocolHandler.value}
            disabled={!matrixProtocolHandlerAvailable()}
            onChange={(event) =>
              setMatrixProtocolHandler(event.currentTarget.checked)
            }
          />
          Handle <code>matrix:</code> links
        </label>
        <p class="muted">
          Registers this web origin as a browser handler for{' '}
          <code>matrix:</code> links. Axon also handles{' '}
          <code>https://matrix.to/</code> links clicked inside the app.
        </p>
        {!matrixProtocolHandlerAvailable() && (
          <p class="muted">
            This browser does not support protocol-handler registration.
          </p>
        )}
        {protocolMessage !== null && <p class="muted">{protocolMessage}</p>}
      </section>
      <section class="panel">
        <h2>Accounts</h2>
        <AccountLifecycle />
      </section>
      <DebugSettings />
      <section class="panel">
        <h2>Session</h2>
        <button type="button" class="danger" onClick={() => auth.clearToken()}>
          Sign out
        </button>
        <p class="muted">
          Clear this browser's Axon access and refresh tokens, along with
          everything cached for the session: the room list, message history, and
          the names shown for direct messages. Only your settings on this page
          are kept.
        </p>
      </section>
      <p class="muted">These settings are stored locally, not on the server.</p>
      <footer class="settings-version muted">
        Web client{' '}
        <CopyableText
          text={BUILD_INFO.displayVersion}
          label="web client version"
        >
          <code>{BUILD_INFO.displayVersion}</code>
        </CopyableText>{' '}
        · built{' '}
        <time dateTime={BUILD_INFO.builtAt}>{BUILD_INFO.builtAtLabel}</time>
        {' · '}
        <a href="/licenses">Open-source licenses</a>
        <br />
        <UpdateCheckControl />
      </footer>
    </div>
  )
}

/**
 * Manual "is there a new build?" (ADR 0087). The automatic path is silent by
 * design, which leaves nowhere to confirm that a client *is* current — the
 * question a bug report starts with. This is that place, and it doubles as the
 * escape hatch when the user wants the update now rather than on next idle.
 */
function UpdateCheckControl() {
  const { updates } = useServices()
  const status = updates.status.value
  const message =
    status === 'checking'
      ? 'Checking…'
      : status === 'available'
        ? 'A new version is available.'
        : status === 'current'
          ? 'This is the latest version.'
          : status === 'error'
            ? "Couldn't reach the server."
            : null

  return (
    <>
      <button
        type="button"
        class="ghost settings-update-check"
        disabled={status === 'checking'}
        onClick={() => void updates.check()}
      >
        Check for updates
      </button>
      {message !== null && <span> {message}</span>}
      {status === 'available' && (
        <>
          {' '}
          <button
            type="button"
            onClick={() => reloadNow(browserReloadEnvironment())}
          >
            Reload
          </button>
        </>
      )}
    </>
  )
}

/**
 * Developer / diagnostic toggles. Hidden behind a Debug button so the rest of
 * Settings stays a user-facing page; these three exist to diagnose a device,
 * not to configure day-to-day use.
 */
function DebugSettings() {
  const { settings } = useServices()
  const [open, setOpen] = useState(false)

  return (
    <section class="panel">
      <button
        type="button"
        aria-expanded={open}
        aria-controls={open ? 'debug-settings' : undefined}
        onClick={() => setOpen((current) => !current)}
      >
        Debug
      </button>
      {open && (
        <div id="debug-settings" class="settings-debug-body">
          <label class="setting-row">
            <input
              type="checkbox"
              checked={settings.developerMode.value}
              onChange={(event) =>
                (settings.developerMode.value = event.currentTarget.checked)
              }
            />
            Developer mode
          </label>
          <p class="muted">
            Adds per-event diagnostics to the message list. Inspect panels show
            decrypted event content.
          </p>
          <label class="setting-row">
            <input
              type="checkbox"
              checked={settings.perfMarks.value}
              onChange={(event) =>
                (settings.perfMarks.value = event.currentTarget.checked)
              }
            />
            Performance instrumentation
          </label>
          <p class="muted">
            Records timing marks and draws a live scroll-anchoring readout over
            the app — the numbers a screen recording needs on a phone, where
            there is no console to read marks from.
          </p>
          <label class="setting-row">
            <input
              type="checkbox"
              checked={settings.pageScrollReset.value}
              onChange={(event) =>
                (settings.pageScrollReset.value = event.currentTarget.checked)
              }
            />
            Correct iOS keyboard page drift
          </label>
          <p class="muted">
            Off by default. On, the app snaps the page back when iOS Safari
            scrolls it behind the keyboard — which measurably causes the shell
            to jitter while scrolling, because the snap and Safari fight each
            frame. The layout is already correct without it. Turn it on only to
            compare.
          </p>
        </div>
      )}
    </section>
  )
}

function InstallAppSettings() {
  const { settings } = useServices()
  const [installing, setInstalling] = useState(false)
  const platform = detectInstallPlatform()
  const copy = installCopy(platform)
  const installed = isInstalledDisplay()
  const badgeAvailable = appBadgeAvailable()
  const needsNotificationPermission =
    badgeAvailable &&
    notificationPermissionAvailable() &&
    badgeNeedsNotificationPermission()
  const [notificationPermission, setNotificationPermission] =
    useState<NotificationPermission | null>(
      notificationPermissionAvailable() ? Notification.permission : null,
    )

  const install = async () => {
    setInstalling(true)
    try {
      await promptInstallApp()
    } finally {
      setInstalling(false)
    }
  }

  const requestBadgePermission = () => {
    // Must run synchronously inside this click handler, with no `await`
    // ahead of it — Safari only honors `Notification.requestPermission()`
    // from a real user gesture (ADR 0080).
    const request = requestAppBadgeNotificationPermission()
    if (request !== null) {
      void request.then(setNotificationPermission)
    }
  }

  return (
    <section class="panel">
      <h2>{copy.heading}</h2>
      {installed ? (
        <p class="muted">{copy.installed}</p>
      ) : installPromptAvailable.value ? (
        <>
          <button type="button" onClick={() => void install()}>
            {installing ? 'Opening…' : copy.button}
          </button>
          <InstallOutcomeMessage />
        </>
      ) : installOutcome.value !== 'idle' ? (
        <InstallOutcomeMessage />
      ) : platform === 'ios' ? (
        <ol class="install-steps">
          <li>Tap the Share button in Safari.</li>
          <li>Choose Add to Home Screen.</li>
          <li>Tap Add.</li>
        </ol>
      ) : platform === 'android' ? (
        <p class="muted">
          Open your browser menu and choose Add to home screen. Chrome will also
          show an install button here when it makes the prompt available.
        </p>
      ) : (
        <p class="muted">{copy.unavailable}</p>
      )}
      <label class="setting-row">
        <input
          type="checkbox"
          checked={settings.appBadgeEnabled.value}
          onChange={(event) =>
            (settings.appBadgeEnabled.value = event.currentTarget.checked)
          }
        />
        Show unread count on the app icon
      </label>
      <p class="muted">
        Badges the app icon with the number of unread messages while installed
        and open in the background. On by default.
      </p>
      {!badgeAvailable && (
        <p class="muted">
          Not available in this browser right now — some browsers (Safari on
          iOS/iPadOS) only support this once Axon is added to your home screen
          and reopened from there. The setting is saved either way and takes
          effect as soon as it's supported.
        </p>
      )}
      {needsNotificationPermission && notificationPermission === 'default' && (
        <>
          <button type="button" onClick={requestBadgePermission}>
            Allow notifications to enable the badge
          </button>
          <p class="muted">
            Safari only displays this badge once notification permission is
            granted, even though Axon doesn't send notifications. This asks for
            that permission — nothing else changes.
          </p>
        </>
      )}
      {needsNotificationPermission && notificationPermission === 'denied' && (
        <p class="muted">
          Notification permission was denied, so this badge won't appear. Enable
          notifications for Axon in your device's system settings, then reopen
          the app.
        </p>
      )}
    </section>
  )
}

function InstallOutcomeMessage() {
  switch (installOutcome.value) {
    case 'accepted':
      return <p class="muted">Install request accepted.</p>
    case 'dismissed':
      return <p class="muted">Install request dismissed.</p>
    case 'error':
      return <p class="muted">Install prompt could not be opened.</p>
    default:
      return null
  }
}

type InstallPlatform =
  'android' | 'ios' | 'linux' | 'macos' | 'windows' | 'other'

interface InstallCopy {
  heading: string
  button: string
  installed: string
  unavailable: string
}

function detectInstallPlatform(): InstallPlatform {
  const platform = currentPlatform().toLowerCase()
  const userAgent = navigator.userAgent.toLowerCase()
  const touchPoints = navigator.maxTouchPoints ?? 0
  if (/android/.test(userAgent)) {
    return 'android'
  }
  if (/\b(iphone|ipad|ipod)\b/.test(userAgent)) {
    return 'ios'
  }
  if (/win/.test(platform) || /windows/.test(userAgent)) {
    return 'windows'
  }
  if (isApplePlatform(platform, touchPoints)) {
    return 'macos'
  }
  if (/linux|x11/.test(platform) || /linux|x11/.test(userAgent)) {
    return 'linux'
  }
  return 'other'
}

function installCopy(platform: InstallPlatform): InstallCopy {
  switch (platform) {
    case 'android':
    case 'ios':
      return {
        heading: 'Home screen',
        button: 'Add to home screen',
        installed: 'Axon is already running from your home screen.',
        unavailable:
          'Home-screen install is available from supported mobile browsers.',
      }
    case 'windows':
      return {
        heading: 'Desktop app',
        button: 'Add to Start Menu',
        installed: 'Axon is already available from your Start menu.',
        unavailable:
          'Desktop app install is available from supported browsers.',
      }
    case 'macos':
      return {
        heading: 'Desktop app',
        button: 'Add to Applications',
        installed: 'Axon is already available from Applications.',
        unavailable:
          'Desktop app install is available from supported browsers.',
      }
    case 'linux':
      return {
        heading: 'Desktop app',
        button: 'Install desktop app',
        installed: 'Axon is already available from your app launcher.',
        unavailable:
          'Desktop app install is available from supported browsers.',
      }
    default:
      return {
        heading: 'Install app',
        button: 'Install Axon',
        installed: 'Axon is already installed as an app.',
        unavailable: 'App install is available from supported browsers.',
      }
  }
}

function isInstalledDisplay(): boolean {
  const navigatorStandalone = (
    navigator as Navigator & { standalone?: boolean }
  ).standalone
  return (
    navigatorStandalone === true ||
    window.matchMedia?.('(display-mode: standalone)').matches === true
  )
}
