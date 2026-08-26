import { useEffect, useRef, useState } from 'preact/hooks'
import { copyText } from '../copy-text'
import { matrixToEventLink } from '../matrix-to'
import type { TimeFormat } from '../stores/settings'
import type { TimelineEvent, TimelineStore } from '../stores/timeline'

/**
 * The per-row send-state fragments shared by the main timeline and the
 * thread panel (WCR-16) — they were copy-pasted and had already started
 * life as two identical blocks.
 */

/**
 * The row's timestamp. Hand-rolled rather than `toLocaleTimeString` — the
 * locale path showed up in Safari profiles on older iPhones — with the
 * format a setting (Settings → Timeline) since hardcoding 12-hour took
 * locale-appropriate 24-hour rendering away from those locales.
 */
export function formatEventTime(
  originTs: number,
  format: TimeFormat = '12h',
): string {
  const date = new Date(originTs)
  const hours = date.getHours()
  const minutes = date.getMinutes().toString().padStart(2, '0')
  if (format === '24h') {
    return `${hours.toString().padStart(2, '0')}:${minutes}`
  }
  const hour12 = hours % 12 || 12
  const period = hours >= 12 ? 'pm' : 'am'
  return `${hour12}:${minutes}${period}`
}

export function EventTime({
  event,
  format,
}: {
  event: TimelineEvent
  format?: TimeFormat
}) {
  const [copyStatus, setCopyStatus] = useState<'idle' | 'copied' | 'failed'>(
    'idle',
  )
  const clearCopyStatus = useRef<number | null>(null)
  useEffect(() => {
    return () => {
      if (clearCopyStatus.current !== null) {
        window.clearTimeout(clearCopyStatus.current)
      }
    }
  }, [])
  if (event.localEcho?.status === 'pending') {
    return <span class="muted local-echo-status">Sending…</span>
  }
  const formatted = formatEventTime(event.origin_ts, format)
  if (event.localEcho !== undefined || event.event_id.startsWith('local:')) {
    return (
      <time class="muted" dateTime={new Date(event.origin_ts).toISOString()}>
        {formatted}
      </time>
    )
  }
  const copyLink = async () => {
    if (clearCopyStatus.current !== null) {
      window.clearTimeout(clearCopyStatus.current)
      clearCopyStatus.current = null
    }
    const ok = await copyText(matrixToEventLink(event.room_id, event.event_id))
    setCopyStatus(ok ? 'copied' : 'failed')
    clearCopyStatus.current = window.setTimeout(() => {
      setCopyStatus('idle')
      clearCopyStatus.current = null
    }, 1800)
  }
  const title =
    copyStatus === 'copied'
      ? 'Event link copied'
      : copyStatus === 'failed'
        ? 'Could not copy event link'
        : 'Copy Matrix.to link to event'
  return (
    <>
      <button
        type="button"
        class={`event-time-copy muted${copyStatus === 'failed' ? ' failed' : ''}`}
        title={title}
        aria-label="Copy Matrix.to link to event"
        onClick={() => void copyLink()}
      >
        <time dateTime={new Date(event.origin_ts).toISOString()}>
          {formatted}
        </time>
      </button>
      {copyStatus !== 'idle' && (
        <span
          class={`event-copy-status${copyStatus === 'failed' ? ' error' : ''}`}
          role="status"
        >
          {copyStatus === 'copied' ? 'Copied' : 'Copy failed'}
        </span>
      )}
    </>
  )
}

/** The failed-send notice with its Retry/Discard controls. */
export function FailedSend({
  event,
  timeline,
}: {
  event: TimelineEvent
  timeline: TimelineStore
}) {
  if (event.localEcho?.status !== 'failed') {
    return null
  }
  return (
    <span class="local-echo-status error">
      Failed to send
      <button
        type="button"
        class="ghost"
        onClick={() => void timeline.retrySend(event.event_id)}
      >
        Retry
      </button>
      <button
        type="button"
        class="ghost"
        onClick={() => timeline.discardSend(event.event_id)}
      >
        Discard
      </button>
    </span>
  )
}
