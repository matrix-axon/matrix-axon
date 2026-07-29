import { useServices } from '../services'
import type { ConnectionState } from '../stores/live-connection'

/** Human label per live-socket state (M-W6, ADR 0061). */
const LABELS: Record<ConnectionState, string> = {
  connecting: 'Connecting…',
  live: 'Live',
  reconnecting: 'Reconnecting…',
  offline: 'Offline',
}

/**
 * The live-socket status shown in the shell header (M-W6, ADR 0061) — the
 * visible half of `LiveConnection.connection`. A colored dot plus a label:
 * green `live`, amber `connecting`/`reconnecting` (the dot pulses while
 * retrying), red `offline`. `role="status"` announces transitions to assistive
 * tech without stealing focus.
 */
export function ConnectionIndicator() {
  const { live } = useServices()
  const state = live.connection.value
  return (
    <span
      class={`conn conn-${state}`}
      role="status"
      title={`WebSocket: ${LABELS[state]}`}
    >
      <span class="conn-dot" aria-hidden="true" />
      <span class="conn-label">{LABELS[state]}</span>
    </span>
  )
}
