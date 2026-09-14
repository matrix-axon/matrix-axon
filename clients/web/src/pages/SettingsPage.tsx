import { useLocation } from 'preact-iso'
import { useEffect, useRef, useState } from 'preact/hooks'
import {
  appBadgeAvailable,
  badgeNeedsNotificationPermission,
  notificationPermissionAvailable,
  requestAppBadgeNotificationPermission,
} from '../app-badge'
import { BUILD_INFO } from '../build-info'
import { CopyableText } from '../components/CopyableText'
import { ReactionPicker } from '../components/MessageEventRow'
import { useMobileSwipeBack } from '../components/use-mobile-swipe-back'
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
import { disconnectFromServer } from '../server-url'
import { formatTelemetry } from '../stores/telemetry'
import { resolveApiBaseUrl, useServices } from '../services'
import { currentPlatform, isApplePlatform } from '../shortcuts'
import type {
  StateEventVisibility,
  Theme,
  TimeFormat,
} from '../stores/settings'
import {
  assignMessageGestureAction,
  defaultMessageGestures,
  isSingleEmoji,
  MESSAGE_GESTURE_ACTIONS,
  type MessageGesture,
  type MessageGestureAction,
  type MessageGesturePreferences,
} from '../stores/message-gestures'

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

const MESSAGE_GESTURES: {
  value: MessageGesture
  label: string
}[] = [
  { value: 'double_tap', label: 'Double tap' },
  { value: 'touch_and_hold', label: 'Touch and hold' },
  { value: 'swipe_left', label: 'Swipe left' },
]

const MESSAGE_GESTURE_ACTION_LABELS: Record<MessageGestureAction, string> = {
  reply: 'Reply',
  thread: 'Open thread',
  react: 'React',
  edit: 'Edit',
  delete: 'Delete',
}

/** Theme + (schema-versioned) local settings (ADR 0046, M-W3). */
export function SettingsPage() {
  const location = useLocation()
  const settingsPane = useRef<HTMLDivElement>(null)
  const mobileSwipeBack = useMobileSwipeBack<HTMLDivElement>({
    getPane: () => settingsPane.current,
    onBack: () => location.route('/'),
  })

  return (
    <div class="settings-back-surface mobile-back-surface" {...mobileSwipeBack}>
      <span class="mobile-back-affordance" aria-hidden="true">
        <span>‹</span>
        Rooms
      </span>
      <div ref={settingsPane} class="settings-back-pane">
        <SettingsPageContents />
      </div>
    </div>
  )
}

function SettingsPageContents() {
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
        <MessageGestureSettings />
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
        <p>
          Add Matrix accounts, choose the active account, and manage recovery,
          verification, logout, and deletion on the dedicated accounts page.
        </p>
        <a href="/accounts" class="button-link">
          Manage accounts
        </a>
      </section>
      <ServerSettings />
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
      <p class="muted">
        Gestures sync through Axon. Other settings on this page are stored only
        in this client.
      </p>
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

function cloneMessageGestures(
  value: MessageGesturePreferences,
): MessageGesturePreferences {
  return {
    schema_version: 1,
    bindings: { ...value.bindings },
    reaction_emoji: value.reaction_emoji,
  }
}

function sameMessageGestures(
  left: MessageGesturePreferences,
  right: MessageGesturePreferences,
): boolean {
  return (
    left.reaction_emoji === right.reaction_emoji &&
    MESSAGE_GESTURES.every(
      ({ value }) => left.bindings[value] === right.bindings[value],
    )
  )
}

function duplicateMessageGestureAction(
  value: MessageGesturePreferences,
): boolean {
  const actions = Object.values(value.bindings).filter(
    (action): action is MessageGestureAction => action !== null,
  )
  return new Set(actions).size !== actions.length
}

function MessageGestureSettings() {
  const { messageGestures, settings } = useServices()
  const current = messageGestures.preferences.value
  const revision = messageGestures.revision.value
  const [draft, setDraft] = useState(defaultMessageGestures)
  const [pickerOpen, setPickerOpen] = useState(false)
  const [helpOpen, setHelpOpen] = useState(false)
  const [saveStatus, setSaveStatus] = useState<string | null>(null)
  const saveRequest = useRef(0)
  const helpContainer = useRef<HTMLDivElement>(null)
  const helpButton = useRef<HTMLButtonElement>(null)

  const closeHelp = () => {
    setHelpOpen(false)
    helpButton.current?.focus()
  }

  useEffect(() => {
    void messageGestures.hydrate()
  }, [messageGestures])

  useEffect(() => {
    if (!helpOpen) {
      return
    }
    const closeOnOutsidePointer = (event: PointerEvent) => {
      if (
        event.target instanceof Node &&
        !helpContainer.current?.contains(event.target)
      ) {
        setHelpOpen(false)
      }
    }
    document.addEventListener('pointerdown', closeOnOutsidePointer)
    return () =>
      document.removeEventListener('pointerdown', closeOnOutsidePointer)
  }, [helpOpen])

  useEffect(() => {
    if (current === null) {
      return
    }
    setDraft(cloneMessageGestures(current))
  }, [current, revision])

  const autosave = (
    next: MessageGesturePreferences,
    statusAfterSave = 'Gestures saved',
  ) => {
    const attempted = cloneMessageGestures(next)
    const request = ++saveRequest.current
    setDraft(attempted)
    setSaveStatus(null)
    void messageGestures.save(attempted).then((ok) => {
      if (ok && request === saveRequest.current) {
        const winner = messageGestures.preferences.peek()
        setSaveStatus(
          winner !== null && !sameMessageGestures(winner, attempted)
            ? 'Another device updated gestures'
            : statusAfterSave,
        )
      }
    })
  }

  const updateBinding = (
    gesture: MessageGesture,
    action: MessageGestureAction | null,
  ) => {
    const assignment = assignMessageGestureAction(draft, gesture, action)
    if (assignment.displaced === null) {
      autosave(assignment.value)
      return
    }
    const displacedLabel = MESSAGE_GESTURES.find(
      ({ value }) => value === assignment.displaced,
    )!.label
    const replacementLabel =
      assignment.replacement === null
        ? 'Off'
        : MESSAGE_GESTURE_ACTION_LABELS[assignment.replacement]
    autosave(
      assignment.value,
      `${displacedLabel} changed to ${replacementLabel}`,
    )
  }

  const updateEmoji = (emoji: string) => {
    autosave({ ...draft, reaction_emoji: emoji })
    setPickerOpen(false)
  }

  const invalid =
    duplicateMessageGestureAction(draft) || !isSingleEmoji(draft.reaction_emoji)
  const loading =
    current === null &&
    (messageGestures.status.value === 'idle' ||
      messageGestures.status.value === 'loading')

  return (
    <div class="message-gesture-settings">
      <div class="message-gesture-heading">
        <h3 class="settings-group-label">Gestures</h3>
        <div
          class="message-gesture-help"
          ref={helpContainer}
          onKeyDown={(event) => {
            if (event.key === 'Escape' && helpOpen) {
              event.preventDefault()
              closeHelp()
            }
          }}
        >
          <button
            type="button"
            class="message-gesture-help-button"
            ref={helpButton}
            aria-label="About gestures"
            aria-expanded={helpOpen}
            aria-controls="mobile-gesture-help"
            title="About gestures"
            onClick={() => setHelpOpen((open) => !open)}
          >
            <span aria-hidden="true">i</span>
          </button>
          {helpOpen && (
            <div
              id="mobile-gesture-help"
              class="message-gesture-help-popover"
              role="note"
              aria-label="Gesture help"
            >
              <p>
                Gesture settings sync across clients. Swipe right returns from
                threads to messages or from messages to rooms. Tap or click a
                timestamp to copy a link to the message. Long tap (or
                double-click) a timestamp to copy the text of the message. Set
                Double tap to Off to restore native double-tap/double-click word
                selection.
              </p>
              <button type="button" class="ghost" onClick={closeHelp}>
                Close
              </button>
            </div>
          )}
        </div>
      </div>
      <p class="muted message-gesture-swap-help">
        Choosing an action already used by another gesture swaps their
        assignments.
      </p>
      {loading ? (
        <p class="muted" role="status">
          Loading gestures…
        </p>
      ) : (
        <>
          <div class="message-gesture-bindings">
            {MESSAGE_GESTURES.map(({ value: gesture, label }) => (
              <label key={gesture}>
                {label}
                <select
                  value={draft.bindings[gesture] ?? ''}
                  onChange={(event) =>
                    updateBinding(
                      gesture,
                      event.currentTarget.value === ''
                        ? null
                        : (event.currentTarget.value as MessageGestureAction),
                    )
                  }
                >
                  <option value="">Off</option>
                  {MESSAGE_GESTURE_ACTIONS.map((action) => (
                    <option key={action} value={action}>
                      {MESSAGE_GESTURE_ACTION_LABELS[action]}
                    </option>
                  ))}
                </select>
              </label>
            ))}
            <div class="message-gesture-fixed">
              <span>Swipe right</span>
              <span>Go back</span>
            </div>
          </div>
          <div class="message-gesture-emoji">
            <span>Default reaction</span>
            <button
              type="button"
              class="message-gesture-emoji-button"
              aria-label={`Choose reaction emoji, currently ${draft.reaction_emoji}`}
              aria-expanded={pickerOpen}
              onClick={() => setPickerOpen((open) => !open)}
            >
              <span aria-hidden="true">{draft.reaction_emoji}</span>
            </button>
          </div>
          {pickerOpen && (
            <ReactionPicker
              ariaLabel="Choose gesture reaction"
              settings={settings}
              onClose={() => setPickerOpen(false)}
              onReact={updateEmoji}
            />
          )}
          {draft.bindings.touch_and_hold === null && (
            <p class="muted">
              Native text selection and link previews are available while touch
              and hold is Off. Timestamp hold-to-copy is also disabled.
            </p>
          )}
          {messageGestures.error.value !== null && (
            <div class="message-gesture-conflict error" role="alert">
              <span>Could not sync gestures.</span>
              <button
                type="button"
                class="ghost"
                disabled={messageGestures.saving.value}
                onClick={() => autosave(draft)}
              >
                Retry
              </button>
            </div>
          )}
          {invalid && (
            <p class="error" role="alert">
              Choose one emoji and assign each action at most once.
            </p>
          )}
          <div class="message-gesture-settings-actions">
            <button
              type="button"
              class="ghost"
              disabled={invalid}
              onClick={() =>
                autosave(defaultMessageGestures(), 'Defaults restored')
              }
            >
              Restore defaults
            </button>
          </div>
          {(messageGestures.saving.value || saveStatus !== null) && (
            <p role="status">
              {messageGestures.saving.value ? 'Saving gestures…' : saveStatus}
            </p>
          )}
        </>
      )}
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
/**
 * Keeping the performance summaries, and getting them off the device.
 *
 * The overlay only helps when someone is watching: the slow load this
 * instrumentation was built for has never happened while a screen recording
 * was running. Persisting the summaries removes that requirement, and Copy is
 * how they leave a phone that has no console and no usable file download in
 * standalone mode.
 */
function TelemetrySettings() {
  const { settings, telemetry } = useServices()
  const [status, setStatus] = useState<string | null>(null)

  async function withText(
    hand: (text: string) => Promise<void>,
    done: string,
  ): Promise<void> {
    try {
      const text = formatTelemetry(await telemetry.read())
      await hand(text)
      setStatus(done)
    } catch {
      // Clipboard and share both reject when the gesture is not trusted or the
      // user dismisses the sheet. Neither is an error worth a banner.
      setStatus('Could not share the telemetry.')
    }
  }

  return (
    <>
      <label class="setting-row">
        <input
          type="checkbox"
          checked={settings.persistTelemetry.value}
          onChange={(event) =>
            (settings.persistTelemetry.value = event.currentTarget.checked)
          }
        />
        Keep performance summaries on this device
      </label>
      <p class="muted">
        Stores the summary lines — timings only, no room or account identifiers
        — so a slow load can be read back afterwards instead of needing a screen
        recording at the moment it happens. Requires performance
        instrumentation. Cleared on sign-out.
      </p>
      <div class="setting-row">
        <button
          type="button"
          onClick={() =>
            void withText(
              (text) => navigator.clipboard.writeText(text),
              'Copied.',
            )
          }
        >
          Copy telemetry
        </button>
        {typeof navigator.share === 'function' && (
          <button
            type="button"
            onClick={() =>
              void withText((text) => navigator.share({ text }), 'Shared.')
            }
          >
            Share
          </button>
        )}
        <button
          type="button"
          onClick={() => {
            void telemetry.clear().then(() => setStatus('Cleared.'))
          }}
        >
          Clear
        </button>
      </div>
      {status !== null && (
        <p class="muted" aria-live="polite">
          {status}
        </p>
      )}
    </>
  )
}

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
              checked={settings.perfOverlay.value}
              onChange={(event) =>
                (settings.perfOverlay.value = event.currentTarget.checked)
              }
            />
            Show the live readout on screen
          </label>
          <p class="muted">
            Draws the numbers over the app. Turn this off to record during
            ordinary use — the summaries are still collected, and still kept
            below if that is enabled.
          </p>
          <TelemetrySettings />
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

/**
 * Which server this client is pointed at, and how to change it (ADR 0102 § 3).
 *
 * Hidden wherever the platform has a same-origin default — i.e. every browser
 * deployment, where the server is not a choice the user made and offering to
 * "change" it would be offering to break the app. Only a packaged build, which
 * asked for the address on first run, can act on this.
 *
 * Changing it reloads rather than rebuilding in place: the base URL is baked
 * into the service graph at construction (`createServices`), and there is no
 * mechanism — nor any reason to build one — for swapping it under a live
 * socket, an open timeline and a warm cache. Navigating to `/` first is
 * deliberate: a reload at a deep room path would be a route belonging to the
 * *old* server.
 */
function ServerSettings() {
  // From the graph, not `browserPlatform()`. Constructing one here always sees
  // the browser's `'/'` default and hides this panel — including in the shell,
  // which is the only build that can reach it.
  const { auth, platform } = useServices()
  if (platform.defaultApiBaseUrl !== null) {
    return null
  }
  // Through the graph's platform, not a fresh `browserPlatform()`: with nothing
  // stored and nothing baked in, the browser answers `'/'` and this would read
  // "Server: /" — the browser's same-origin default, in the one build that has
  // no same-origin server. Storage stays the window's because the graph does
  // not expose its own, and `createServices` is constructed with that same one.
  const current = resolveApiBaseUrl(undefined, platform)
  return (
    <section class="panel">
      <h2>Server</h2>
      <p class="muted">{current ?? 'No server configured.'}</p>
      <button
        type="button"
        class="danger"
        onClick={() => {
          // Credentials go with the server that issued them; see
          // `disconnectFromServer`.
          disconnectFromServer(window.localStorage, () => auth.clearToken())
          window.location.assign('/')
        }}
      >
        Change server
      </button>
      <p class="muted">
        Disconnect from this server and choose another. This signs you out:
        credentials belong to the server that issued them, so you will sign in
        again even if you switch back.
      </p>
    </section>
  )
}
