import { useEffect, useRef, useState } from 'preact/hooks'
import { copyText, type CopySource } from '../copy-text'
import { matrixToEventLink } from '../matrix-to'
import type { TimeFormat } from '../stores/settings'
import type { TimelineEvent, TimelineStore } from '../stores/timeline'

const DESKTOP_DOUBLE_CLICK_MS = 300
/**
 * How long a completed hold waits, after release, for the click that WebKit
 * counts as a user activation before copying without one.
 */
const HOLD_CLICK_WAIT_MS = 400

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
  touchHoldEnabled = false,
}: {
  event: TimelineEvent
  format?: TimeFormat
  touchHoldEnabled?: boolean
}) {
  const [copyStatus, setCopyStatus] = useState<
    'idle' | 'link-copied' | 'text-copied' | 'no-text' | 'failed'
  >('idle')
  const clearCopyStatus = useRef<number | null>(null)
  const linkClickTimer = useRef<number | null>(null)
  const holdTimer = useRef<number | null>(null)
  const holdCopyTimer = useRef<number | null>(null)
  const holdPointer = useRef<{
    id: number
    x: number
    y: number
    completed: boolean
  } | null>(null)
  const suppressNextClick = useRef(false)
  const lastPointerType = useRef<string | null>(null)
  useEffect(() => {
    return () => {
      if (clearCopyStatus.current !== null) {
        window.clearTimeout(clearCopyStatus.current)
      }
      if (linkClickTimer.current !== null) {
        window.clearTimeout(linkClickTimer.current)
      }
      if (holdTimer.current !== null) window.clearTimeout(holdTimer.current)
      if (holdCopyTimer.current !== null) {
        window.clearTimeout(holdCopyTimer.current)
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
  const reportCopy = (
    status: 'link-copied' | 'text-copied' | 'no-text' | 'failed',
  ) => {
    if (clearCopyStatus.current !== null) {
      window.clearTimeout(clearCopyStatus.current)
      clearCopyStatus.current = null
    }
    setCopyStatus(status)
    clearCopyStatus.current = window.setTimeout(() => {
      setCopyStatus('idle')
      clearCopyStatus.current = null
    }, 1800)
  }
  const copyLink = async () => {
    const ok = await copyText(
      matrixToEventLink(event.room_id, event.event_id),
      'link-click',
    )
    reportCopy(ok ? 'link-copied' : 'failed')
  }
  const copyBody = async (source: CopySource) => {
    if (event.body === null || event.body === undefined) {
      reportCopy('no-text')
      return
    }
    const ok = await copyText(event.body, source)
    reportCopy(ok ? 'text-copied' : 'failed')
  }
  const clearLinkClick = () => {
    if (linkClickTimer.current !== null) {
      window.clearTimeout(linkClickTimer.current)
      linkClickTimer.current = null
    }
  }
  const clearHoldCopy = () => {
    if (holdCopyTimer.current !== null) {
      window.clearTimeout(holdCopyTimer.current)
      holdCopyTimer.current = null
    }
  }
  /**
   * A completed hold's copy. Prefer the click that follows release: in
   * WKWebView `pointerup` is not an activating event (a device log showed
   * `activation=false` there, and both `writeText` and `execCommand` were
   * refused), while `click` is. If no click arrives — iOS drops it when the
   * callout takes the gesture — copy anyway after a short wait.
   */
  const scheduleHoldCopy = (source: CopySource) => {
    clearHoldCopy()
    holdCopyTimer.current = window.setTimeout(() => {
      holdCopyTimer.current = null
      void copyBody(source)
    }, HOLD_CLICK_WAIT_MS)
  }
  const clearHold = () => {
    if (holdTimer.current !== null) {
      window.clearTimeout(holdTimer.current)
      holdTimer.current = null
    }
  }
  const title =
    copyStatus === 'link-copied'
      ? 'Event link copied'
      : copyStatus === 'text-copied'
        ? 'Message text copied'
        : copyStatus === 'no-text'
          ? 'No message text to copy'
          : copyStatus === 'failed'
            ? 'Could not copy'
            : 'Click to copy link; double-click to copy message text'
  return (
    <>
      <button
        type="button"
        class={`event-time-copy muted${copyStatus === 'failed' ? ' failed' : ''}`}
        title={title}
        aria-label="Copy link"
        onPointerDown={(pointerEvent) => {
          if (pointerEvent.isPrimary === false) return
          lastPointerType.current = pointerEvent.pointerType
          suppressNextClick.current = false
          if (!touchHoldEnabled || pointerEvent.pointerType === 'mouse') {
            clearHold()
            holdPointer.current = null
            return
          }
          clearHold()
          suppressNextClick.current = false
          holdPointer.current = {
            id: pointerEvent.pointerId,
            x: pointerEvent.clientX,
            y: pointerEvent.clientY,
            completed: false,
          }
          holdTimer.current = window.setTimeout(() => {
            if (holdPointer.current === null) return
            // Only arm the copy here. WebKit grants clipboard access to a
            // gesture's *end* (pointerup / click), not its start, so a write
            // from this timer — 550 ms into a press — is rejected at random.
            // The copy happens on release instead.
            holdPointer.current.completed = true
            suppressNextClick.current = true
          }, 550)
        }}
        onPointerMove={(pointerEvent) => {
          const pointer = holdPointer.current
          if (
            pointer !== null &&
            pointer.id === pointerEvent.pointerId &&
            Math.hypot(
              pointerEvent.clientX - pointer.x,
              pointerEvent.clientY - pointer.y,
            ) > 10
          ) {
            clearHold()
            holdPointer.current = null
          }
        }}
        onPointerUp={(pointerEvent) => {
          const pointer = holdPointer.current
          if (pointer === null || pointer.id !== pointerEvent.pointerId) return
          const completed = pointer.completed
          clearHold()
          holdPointer.current = null
          if (completed) {
            pointerEvent.preventDefault()
            scheduleHoldCopy('body-hold-release')
          }
        }}
        onPointerCancel={(pointerEvent) => {
          const pointer = holdPointer.current
          if (pointer?.id !== pointerEvent.pointerId) return
          clearHold()
          holdPointer.current = null
          // The browser took the gesture over (iOS selection callout) after
          // the hold had already completed: copy anyway, without activation.
          if (pointer.completed) scheduleHoldCopy('body-hold-cancel')
        }}
        onContextMenu={(contextMenu) => {
          if (touchHoldEnabled && holdPointer.current !== null)
            contextMenu.preventDefault()
        }}
        onClick={(click) => {
          if (suppressNextClick.current) {
            suppressNextClick.current = false
            click.preventDefault()
            if (holdCopyTimer.current !== null) {
              clearHoldCopy()
              void copyBody('body-hold-click')
            }
            return
          }
          if (lastPointerType.current === 'mouse' && click.detail === 1) {
            clearLinkClick()
            linkClickTimer.current = window.setTimeout(() => {
              linkClickTimer.current = null
              void copyLink()
            }, DESKTOP_DOUBLE_CLICK_MS)
            return
          }
          if (lastPointerType.current === 'mouse' && click.detail > 1) return
          void copyLink()
        }}
        onDblClick={(click) => {
          if (lastPointerType.current !== 'mouse') return
          clearLinkClick()
          click.preventDefault()
          void copyBody('body-dblclick')
        }}
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
          {copyStatus === 'link-copied'
            ? 'Copied'
            : copyStatus === 'text-copied'
              ? 'Text copied'
              : copyStatus === 'no-text'
                ? 'No message text'
                : 'Copy failed'}
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
