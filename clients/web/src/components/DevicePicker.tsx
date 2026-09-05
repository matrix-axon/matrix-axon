import { useEffect, useState } from 'preact/hooks'
import { useServices } from '../services'
import type { DeviceDto } from '../stores/verification'
import { BodyPortal } from './BodyPortal'
import { useModalFocus } from './use-modal-focus'
import { useShortcuts } from '../shortcuts'

function deviceLabel(device: DeviceDto): string {
  const name = device.display_name?.trim()
  return name !== undefined && name !== '' ? name : 'Unnamed device'
}

export function DevicePicker({
  accountId,
  ownDeviceId,
  onClose,
  onStarted,
}: {
  accountId: string
  ownDeviceId: string | null
  onClose: () => void
  onStarted: (key: string) => void
}) {
  const { verification } = useServices()
  const { containerRef } = useModalFocus<HTMLDivElement>()
  const [pasteOpen, setPasteOpen] = useState(false)
  const [pastedId, setPastedId] = useState('')
  const [startError, setStartError] = useState<string | null>(null)
  const [startingId, setStartingId] = useState<string | null>(null)

  useEffect(() => {
    void verification.loadDevices(accountId)
  }, [accountId, verification])

  const devices = (verification.devicesByAccount.value[accountId] ?? []).filter(
    (device) => ownDeviceId === null || device.device_id !== ownDeviceId,
  )
  const loading = verification.devicesLoading.value[accountId] === true
  const listError = verification.devicesError.value[accountId] ?? null

  const start = async (deviceId: string) => {
    const trimmed = deviceId.trim()
    if (trimmed === '') {
      return
    }
    if (ownDeviceId !== null && trimmed === ownDeviceId) {
      setStartError("That's this Axon session — pick another device.")
      return
    }
    setStartError(null)
    setStartingId(trimmed)
    const result = await verification.start(accountId, trimmed)
    setStartingId(null)
    if (!result.ok) {
      setStartError(result.message)
      return
    }
    onStarted(result.key)
  }

  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onClose()
      },
    },
    { whileTyping: true, capture: true },
  )

  const sorted = [...devices].sort((a, b) => {
    if (a.is_verified !== b.is_verified) {
      return a.is_verified ? -1 : 1
    }
    return deviceLabel(a).localeCompare(deviceLabel(b))
  })

  return (
    <BodyPortal>
      <div
        ref={containerRef}
        class="overlay"
        role="dialog"
        aria-modal="true"
        aria-labelledby="device-picker-title"
      >
        <div class="overlay-panel device-picker">
          <div class="overlay-head">
            <h2 id="device-picker-title">Verify with another device</h2>
            <button type="button" class="ghost" onClick={onClose}>
              Close
            </button>
          </div>
          {(startError ?? listError) !== null && (
            <p class="error" role="alert">
              {startError ?? listError}
            </p>
          )}
          {loading && <p class="muted">Loading devices…</p>}
          {!loading && sorted.length === 0 && (
            <p>
              No other devices yet. If you just signed in on another client,
              wait a moment and retry.
            </p>
          )}
          {sorted.length > 0 && (
            <ul class="device-picker-list">
              {sorted.map((device) => (
                <li key={device.device_id}>
                  <button
                    type="button"
                    class="device-picker-row"
                    disabled={startingId !== null}
                    onClick={() => void start(device.device_id)}
                  >
                    <span class="device-picker-name">
                      {deviceLabel(device)}
                    </span>
                    <code class="device-picker-id">{device.device_id}</code>
                    <span
                      class={`badge ${device.is_verified ? 'verified' : 'muted'}`}
                    >
                      {device.is_verified ? 'verified' : 'not verified'}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
          <div class="dialog-actions">
            <button
              type="button"
              class="ghost"
              onClick={() => void verification.loadDevices(accountId)}
              disabled={loading}
            >
              Retry
            </button>
          </div>
          <details
            class="device-picker-paste"
            open={pasteOpen}
            onToggle={(event) => setPasteOpen(event.currentTarget.open)}
          >
            <summary>Paste a device ID</summary>
            <form
              class="inline-form"
              onSubmit={(event) => {
                event.preventDefault()
                void start(pastedId)
              }}
            >
              <label>
                Device ID
                <input
                  value={pastedId}
                  onInput={(event) => setPastedId(event.currentTarget.value)}
                  autoComplete="off"
                  spellcheck={false}
                />
              </label>
              <button type="submit" disabled={startingId !== null}>
                Start
              </button>
            </form>
          </details>
        </div>
      </div>
    </BodyPortal>
  )
}
