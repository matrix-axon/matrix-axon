import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from 'preact/hooks'
import type { ComponentChildren } from 'preact'
import { EMOJI_PICKER_DATA_SOURCE } from '../emoji'
import {
  isHorizontallyScrollable,
  SWIPE_AXIS_RATIO,
  SWIPE_DECISION_THRESHOLD,
  SWIPE_MIN_X,
  swipeDirection,
} from '../gestures'
import { parseMedia } from '../media/parse-media'
import { localRoomHref, localThreadEventHref } from '../matrix-to'
import { useShortcuts } from '../shortcuts'
import type { SettingsStore } from '../stores/settings'
import type {
  MessageGestureAction,
  MessageGesturePreferences,
} from '../stores/message-gestures'
import type { ThreadsStore } from '../stores/threads'
import type { ReadReceipt } from '../stores/ephemeral'
import {
  inReplyToId,
  type EventDto,
  type TimelineEvent,
  type TimelineStore,
} from '../stores/timeline'
import { EditHistory } from './EditHistory'
import { EventBody } from './EventBody'
import { EventTime, FailedSend } from './EventStatus'
import { UserAvatar } from './UserAvatar'
import { useModalFocus } from './use-modal-focus'
import type { MembersStore } from '../stores/members'
import { EventActionIcon } from './EventActionIcon'
import { useCopyFeedback } from './CopyableText'
import {
  isEditable,
  isMessageActionable,
  isStateEvent,
} from './event-action-eligibility'

export {
  isEditable,
  isReactable,
  isStateEvent,
} from './event-action-eligibility'

/** The quick-react palette (a full picker is available behind `+`). */
export const QUICK_REACTIONS = ['👍', '❤️', '😂', '🎉', '😮', '😢']
const REACTION_TOOLTIP_NAME_LIMIT = 10
const REACTION_TOUCH_HOLD_MS = 450
const EVENT_ACTION_TOUCH_HOLD_MS = 550
/** Finger travel that still counts as a tap on the row, matching the room list. */
const ACTION_ROW_TAP_SLOP_PX = 10
const MESSAGE_DOUBLE_TAP_MS = 300
const MESSAGE_DOUBLE_TAP_SLOP_PX = 24
const MESSAGE_GESTURE_FEEDBACK_MS = 1600
const MESSAGE_REACTION_BURST_MS = 450
const MESSAGE_SWIPE_MAX_X = 96
const MESSAGE_SWIPE_SETTLE_MS = 180

type ReactionTally = NonNullable<EventDto['reactions']>[string]

type GestureReactionPresentation = {
  emoji: string
  removing: boolean
}

type EmojiPickerClickDetail = {
  unicode?: string
  emoji?: { unicode?: string }
}

type PickerPosition = {
  left: number
  top: number
}

interface PickerRect {
  top: number
  right: number
  bottom: number
}

interface PickerSize {
  width: number
  height: number
}

export function fullReactionPickerPosition({
  anchor,
  dialog,
  viewport,
  margin = 8,
  gap = 8,
}: {
  anchor: PickerRect
  dialog: PickerSize
  viewport: PickerSize
  margin?: number
  gap?: number
}): PickerPosition {
  const maxLeft = Math.max(margin, viewport.width - dialog.width - margin)
  const maxTop = Math.max(margin, viewport.height - dialog.height - margin)
  const left = Math.min(Math.max(anchor.right, margin), maxLeft)
  const preferredAbove = anchor.top - dialog.height
  const preferredBelow = anchor.bottom + gap
  const top =
    preferredAbove >= margin
      ? preferredAbove
      : preferredBelow + dialog.height <= viewport.height - margin
        ? preferredBelow
        : Math.min(Math.max(preferredAbove, margin), maxTop)

  return { left, top }
}

/**
 * Can this event be edited? Only our own confirmed, unredacted messages —
 * a state event, a local echo still in flight, and a failed send all have no
 * remote event to replace.
 */
export function isUnsupportedBodylessEvent(event: TimelineEvent): boolean {
  return (
    !isStateEvent(event) &&
    !event.redacted &&
    event.content !== null &&
    event.content !== undefined &&
    !hasVisibleBody(event) &&
    parseMedia(event) === null
  )
}

function EventActionButton({
  label,
  className = 'ghost',
  ariaExpanded,
  onClick,
  children,
}: {
  label: string
  className?: string
  ariaExpanded?: boolean
  onClick: () => void
  children: ComponentChildren
}) {
  const [tooltipOpen, setTooltipOpen] = useState(false)
  const longPressTimer = useRef<number | null>(null)
  const suppressNextClick = useRef(false)

  const clearLongPress = () => {
    if (longPressTimer.current !== null) {
      window.clearTimeout(longPressTimer.current)
      longPressTimer.current = null
    }
  }

  useEffect(
    () => () => {
      clearLongPress()
    },
    [],
  )

  useEffect(() => {
    if (!tooltipOpen) {
      return
    }
    const close = () => setTooltipOpen(false)
    document.addEventListener('touchstart', close)
    document.addEventListener('pointerdown', close)
    document.addEventListener('scroll', close, true)
    document.addEventListener('keydown', close)
    return () => {
      document.removeEventListener('touchstart', close)
      document.removeEventListener('pointerdown', close)
      document.removeEventListener('scroll', close, true)
      document.removeEventListener('keydown', close)
    }
  }, [tooltipOpen])

  return (
    <span class="event-action-tooltip-wrap">
      <button
        type="button"
        class={`${className} event-action-button`}
        aria-expanded={ariaExpanded}
        onTouchStart={() => {
          clearLongPress()
          suppressNextClick.current = false
          longPressTimer.current = window.setTimeout(() => {
            suppressNextClick.current = true
            setTooltipOpen(true)
          }, EVENT_ACTION_TOUCH_HOLD_MS)
        }}
        onTouchMove={clearLongPress}
        onTouchEnd={clearLongPress}
        onTouchCancel={clearLongPress}
        onClick={(event) => {
          if (suppressNextClick.current) {
            event.preventDefault()
            suppressNextClick.current = false
            return
          }
          onClick()
        }}
      >
        {children}
      </button>
      {tooltipOpen && (
        <span class="event-action-tooltip" role="tooltip">
          {label}
        </span>
      )}
    </span>
  )
}

function hasVisibleBody(event: EventDto): boolean {
  return (
    event.body !== null && event.body !== undefined && event.body.trim() !== ''
  )
}

function isRowControl(target: EventTarget | null): boolean {
  return (
    target instanceof Element &&
    target.closest(
      'a, button, input, textarea, select, summary, [contenteditable="true"], [role="button"], [role="textbox"], emoji-picker',
    ) !== null
  )
}

function isInlineLink(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest('a') !== null
}

function isTimestampControl(target: EventTarget | null): boolean {
  return (
    target instanceof Element && target.closest('.event-time-copy') !== null
  )
}

function isMessageBodyTarget(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest('.event-body') !== null
}

function messageGestureActionLabel(action: MessageGestureAction): string {
  switch (action) {
    case 'reply':
      return 'Reply'
    case 'thread':
      return 'Thread'
    case 'react':
      return 'React'
    case 'edit':
      return 'Edit'
    case 'delete':
      return 'Delete'
  }
}

function presentedReactionTally(
  tally: ReactionTally,
  presentation: GestureReactionPresentation | null,
  emoji: string,
  ownUserId: string | null,
): ReactionTally {
  if (presentation?.emoji !== emoji) return tally
  if (presentation.removing) {
    if (!tally.me || tally.count <= 1) return tally
    return {
      ...tally,
      count: tally.count - 1,
      me: false,
      senders:
        ownUserId === null
          ? tally.senders
          : tally.senders.filter((sender) => sender !== ownUserId),
      my_event_ids: [],
    }
  }
  if (tally.me) return tally
  return {
    ...tally,
    count: tally.count + 1,
    me: true,
    senders:
      ownUserId === null || tally.senders.includes(ownUserId)
        ? tally.senders
        : [...tally.senders, ownUserId],
    my_event_ids: [],
  }
}

export function MessageEventRow({
  event,
  timeline,
  threads,
  threadUnread = false,
  members,
  accountId,
  ownUserId,
  readReceipts = [],
  highlighted = false,
  settings,
  messageGestures,
  reactionPickerOpen,
  onSetReactionPicker,
  actionsOpen,
  onOpenActions,
  onReply,
  onEdit,
  onOpenThread,
  showThreadAction = true,
  threadRootId,
  onReplyContextJump,
  onMutation,
}: {
  event: TimelineEvent
  timeline: TimelineStore
  threads?: ThreadsStore
  threadUnread?: boolean
  members: MembersStore
  accountId: string
  ownUserId: string | null
  readReceipts?: readonly ReadReceipt[]
  highlighted?: boolean
  settings: SettingsStore
  messageGestures: MessageGesturePreferences | null
  reactionPickerOpen: boolean
  onSetReactionPicker: (eventId: string | null) => void
  /**
   * Owned by the list, not the row: one open bar at a time. A per-row flag
   * would leave the previous message's actions up when another is tapped.
   */
  actionsOpen: boolean
  onOpenActions: () => void
  onReply: (event: EventDto) => void
  onEdit: (event: EventDto) => void
  onOpenThread?: (rootId: string) => void
  showThreadAction?: boolean
  /** Keep reply-context jumps inside this thread timeline. */
  threadRootId?: string
  /** Let a thread timeline load a reply target without closing its panel. */
  onReplyContextJump?: (eventId: string) => void
  /** Clears transient UI state after a successful event mutation. */
  onMutation?: () => void
}) {
  const rowRef = useRef<HTMLLIElement>(null)
  const [historyOpen, setHistoryOpen] = useState(false)
  const [confirmingRedact, setConfirmingRedact] = useState(false)
  const [inspectOpen, setInspectOpen] = useState(false)
  const gesturePointer = useRef<{
    pointerId: number
    startX: number
    startY: number
    startedAt: number
    direction: 'none' | 'left' | 'right' | 'vertical'
    held: boolean
    linkOnly: boolean
    swipeAction: MessageGestureAction | null
  } | null>(null)
  const holdTimer = useRef<number | null>(null)
  const pendingTap = useRef<{
    x: number
    y: number
    at: number
    timer: number
  } | null>(null)
  const desktopClickTimer = useRef<number | null>(null)
  const lastPointerType = useRef<string | null>(null)
  const suppressNextClick = useRef(false)
  const feedbackTimer = useRef<number | null>(null)
  const revealTimer = useRef<number | null>(null)
  const reactionBurstTimer = useRef<number | null>(null)
  const gestureReactionRequest = useRef<number | null>(null)
  const nextGestureReactionRequest = useRef(0)
  const [gestureFeedback, setGestureFeedback] = useState<string | null>(null)
  const [reactionBurst, setReactionBurst] = useState<string | null>(null)
  const [gestureReaction, setGestureReaction] =
    useState<GestureReactionPresentation | null>(null)
  const [swipeReveal, setSwipeReveal] = useState<MessageGestureAction | null>(
    null,
  )
  const [swipeArmed, setSwipeArmed] = useState(false)
  const [swipeSettling, setSwipeSettling] = useState(false)
  const isState = isStateEvent(event)
  const replyTo = inReplyToId(event)
  const threadSummary = threads?.summaries.value.get(event.event_id)
  const own = ownUserId !== null && event.sender === ownUserId
  const pending = event.localEcho?.status === 'pending'
  const failed = event.localEcho?.status === 'failed'
  const editable = isEditable(event, ownUserId)
  const developerMode = settings.developerMode.value
  const hasMessageActions = isMessageActionable(event)
  const senderDisplay = members.displayName(event.sender)
  const canOpenThread = showThreadAction && onOpenThread !== undefined
  const gestureEligible =
    hasMessageActions && parseMedia(event) === null && messageGestures !== null
  const doubleClickAction = gestureEligible
    ? (messageGestures?.bindings.double_tap ?? null)
    : null
  const touchHoldEnabled =
    gestureEligible && messageGestures.bindings.touch_and_hold !== null
  const visibleReadReceipts = readReceipts.filter(
    (receipt) =>
      receipt.userId !== ownUserId && receipt.userId !== event.sender,
  )
  const reactionEntries = Object.entries(event.reactions ?? {})
  const pendingNewReaction =
    gestureReaction !== null &&
    !gestureReaction.removing &&
    event.reactions?.[gestureReaction.emoji] === undefined

  useEffect(() => {
    if (!actionsOpen) {
      setConfirmingRedact(false)
      // Inspect and edit history are opened from the action bar. On mobile
      // that bar is `display: none` once another row takes it, so leaving
      // these flags set strands the panel with no Hide/close control.
      setInspectOpen(false)
      setHistoryOpen(false)
    }
  }, [actionsOpen])

  useEffect(
    () => () => {
      if (holdTimer.current !== null) window.clearTimeout(holdTimer.current)
      if (pendingTap.current !== null)
        window.clearTimeout(pendingTap.current.timer)
      if (desktopClickTimer.current !== null)
        window.clearTimeout(desktopClickTimer.current)
      if (feedbackTimer.current !== null)
        window.clearTimeout(feedbackTimer.current)
      if (revealTimer.current !== null) window.clearTimeout(revealTimer.current)
      if (reactionBurstTimer.current !== null)
        window.clearTimeout(reactionBurstTimer.current)
      gestureReactionRequest.current = null
    },
    [],
  )

  const clearHoldTimer = () => {
    if (holdTimer.current !== null) {
      window.clearTimeout(holdTimer.current)
      holdTimer.current = null
    }
  }

  const cancelPendingTap = () => {
    if (pendingTap.current !== null) {
      window.clearTimeout(pendingTap.current.timer)
      pendingTap.current = null
    }
  }

  const cancelDesktopClick = () => {
    if (desktopClickTimer.current !== null) {
      window.clearTimeout(desktopClickTimer.current)
      desktopClickTimer.current = null
    }
  }

  const showGestureFeedback = (message: string) => {
    setGestureFeedback(message)
    if (feedbackTimer.current !== null)
      window.clearTimeout(feedbackTimer.current)
    feedbackTimer.current = window.setTimeout(() => {
      setGestureFeedback(null)
      feedbackTimer.current = null
    }, MESSAGE_GESTURE_FEEDBACK_MS)
  }

  const clearSwipePresentation = () => {
    if (revealTimer.current !== null) window.clearTimeout(revealTimer.current)
    revealTimer.current = null
    const row = rowRef.current
    row?.style.removeProperty('--message-swipe-offset')
    row?.style.removeProperty('--message-swipe-reveal-width')
    row?.classList.remove('gesture-swipe-settling')
    setSwipeReveal(null)
    setSwipeArmed(false)
    setSwipeSettling(false)
  }

  const previewSwipeAction = (action: MessageGestureAction, dx: number) => {
    const distance = Math.min(Math.max(-dx, 0), MESSAGE_SWIPE_MAX_X)
    if (revealTimer.current !== null) {
      window.clearTimeout(revealTimer.current)
      revealTimer.current = null
    }
    const row = rowRef.current
    row?.style.setProperty('--message-swipe-offset', `${-distance}px`)
    row?.style.setProperty('--message-swipe-reveal-width', `${distance}px`)
    setSwipeReveal(action)
    setSwipeArmed(-dx >= SWIPE_MIN_X)
    setSwipeSettling(false)
  }

  const settleSwipeAction = () => {
    const row = rowRef.current
    if (row === null || swipeReveal === null) return
    // Put the transition class on the live node before changing the custom
    // properties; the following state update keeps it there across the action's
    // own render (reply/edit state, reaction reconciliation, and so on).
    row.classList.add('gesture-swipe-settling')
    row.style.setProperty('--message-swipe-offset', '0px')
    row.style.setProperty('--message-swipe-reveal-width', '0px')
    setSwipeArmed(false)
    setSwipeSettling(true)
    if (revealTimer.current !== null) window.clearTimeout(revealTimer.current)
    revealTimer.current = window.setTimeout(() => {
      clearSwipePresentation()
      revealTimer.current = null
    }, MESSAGE_SWIPE_SETTLE_MS)
  }

  const runGestureReaction = (emoji: string) => {
    // The event remains unchanged until the authoritative refresh lands. Do
    // not issue a second toggle against that stale snapshot in the meantime.
    if (gestureReactionRequest.current !== null) return
    const request = ++nextGestureReactionRequest.current
    const removing = event.reactions?.[emoji]?.me === true
    gestureReactionRequest.current = request
    setGestureReaction({ emoji, removing })

    if (reactionBurstTimer.current !== null)
      window.clearTimeout(reactionBurstTimer.current)
    if (removing) {
      setReactionBurst(null)
    } else {
      setReactionBurst(emoji)
      reactionBurstTimer.current = window.setTimeout(() => {
        setReactionBurst(null)
        reactionBurstTimer.current = null
      }, MESSAGE_REACTION_BURST_MS)
    }

    void timeline.toggleReaction(event, emoji).then((ok) => {
      if (gestureReactionRequest.current !== request) return
      gestureReactionRequest.current = null
      setGestureReaction(null)
      if (ok) {
        onMutation?.()
      } else {
        if (reactionBurstTimer.current !== null) {
          window.clearTimeout(reactionBurstTimer.current)
          reactionBurstTimer.current = null
        }
        setReactionBurst(null)
        showGestureFeedback('Reaction could not be updated')
      }
    })
  }

  const runGestureAction = (action: MessageGestureAction): void => {
    switch (action) {
      case 'reply':
        onReply(event)
        return
      case 'thread':
        if (!canOpenThread) {
          showGestureFeedback('Thread is unavailable for this message')
          return
        }
        onOpenThread?.(event.event_id)
        return
      case 'react':
        runGestureReaction(messageGestures?.reaction_emoji ?? '👍')
        return
      case 'edit':
        if (!editable) {
          showGestureFeedback('Edit is unavailable for this message')
          return
        }
        onEdit(event)
        return
      case 'delete':
        if (!own) {
          showGestureFeedback('Delete is unavailable for this message')
          return
        }
        onOpenActions()
        setConfirmingRedact(true)
    }
  }

  const finishTap = (x: number, y: number): void => {
    const action = gestureEligible
      ? (messageGestures?.bindings.double_tap ?? null)
      : null
    if (action === null) {
      onOpenActions()
      return
    }
    const now = performance.now()
    const first = pendingTap.current
    if (
      first !== null &&
      now - first.at <= MESSAGE_DOUBLE_TAP_MS &&
      Math.hypot(x - first.x, y - first.y) <= MESSAGE_DOUBLE_TAP_SLOP_PX
    ) {
      window.clearTimeout(first.timer)
      pendingTap.current = null
      runGestureAction(action)
      return
    }
    if (first !== null) window.clearTimeout(first.timer)
    const timer = window.setTimeout(() => {
      pendingTap.current = null
      onOpenActions()
    }, MESSAGE_DOUBLE_TAP_MS)
    pendingTap.current = { x, y, at: now, timer }
  }

  return (
    <li
      ref={rowRef}
      class={`event-row${isState ? ' state-event' : ''}${highlighted ? ' highlighted' : ''}${pending ? ' pending' : ''}${failed ? ' failed' : ''}${actionsOpen ? ' actions-open' : ''}${touchHoldEnabled ? ' touch-hold-enabled' : ''}${swipeReveal !== null ? ' gesture-swipe-reveal' : ''}${swipeArmed ? ' gesture-swipe-armed' : ''}${swipeSettling ? ' gesture-swipe-settling' : ''}`}
      data-event-id={event.event_id}
      onPointerDown={(pointerEvent) => {
        lastPointerType.current = pointerEvent.pointerType
        // Same reason the thread badge opens on pointerdown: iOS does not
        // synthesize `click` on a non-button `<li>`, and mobile CSS hides the
        // hover bar, so a real tap would otherwise do nothing. Mouse keeps
        // `click` below. Scroll is filtered on move/cancel, like the room list.
        if (pointerEvent.pointerType === 'mouse') {
          clearHoldTimer()
          gesturePointer.current = null
          rowRef.current?.classList.remove('touch-gesture-active')
          return
        }
        cancelDesktopClick()
        if (pendingTap.current !== null) {
          // A second contact may become the second tap, a hold, or a drag.
          // Keep its first-tap coordinates for now, but do not let the delayed
          // action bar open underneath the new gesture.
          window.clearTimeout(pendingTap.current.timer)
        }
        if (
          gesturePointer.current !== null &&
          gesturePointer.current.pointerId !== pointerEvent.pointerId
        ) {
          clearHoldTimer()
          gesturePointer.current = null
          return
        }
        clearHoldTimer()
        gesturePointer.current = null
        suppressNextClick.current = false
        clearSwipePresentation()
        const linkOnly = isInlineLink(pointerEvent.target)
        if (isRowControl(pointerEvent.target) && !linkOnly) return
        if (isHorizontallyScrollable(pointerEvent.target)) return
        if (linkOnly && !touchHoldEnabled) return
        if (touchHoldEnabled) {
          rowRef.current?.classList.add('touch-gesture-active')
        }
        gesturePointer.current = {
          pointerId: pointerEvent.pointerId,
          startX: pointerEvent.clientX,
          startY: pointerEvent.clientY,
          startedAt: performance.now(),
          direction: 'none',
          held: false,
          linkOnly,
          swipeAction:
            gestureEligible && !linkOnly
              ? (messageGestures?.bindings.swipe_left ?? null)
              : null,
        }
        const holdAction = gestureEligible
          ? (messageGestures?.bindings.touch_and_hold ?? null)
          : null
        if (holdAction !== null) {
          holdTimer.current = window.setTimeout(() => {
            const gesture = gesturePointer.current
            if (gesture === null || gesture.direction !== 'none') return
            cancelPendingTap()
            gesture.held = true
            suppressNextClick.current = true
            runGestureAction(holdAction)
          }, EVENT_ACTION_TOUCH_HOLD_MS)
        }
      }}
      onPointerMove={(pointerEvent) => {
        const gesture = gesturePointer.current
        if (gesture === null || gesture.pointerId !== pointerEvent.pointerId) {
          return
        }
        const dx = pointerEvent.clientX - gesture.startX
        const dy = pointerEvent.clientY - gesture.startY
        if (gesture.direction === 'left') {
          if (gesture.swipeAction !== null) {
            previewSwipeAction(gesture.swipeAction, dx)
            pointerEvent.preventDefault()
          }
          return
        }
        if (gesture.direction !== 'none') return
        const absX = Math.abs(dx)
        const absY = Math.abs(dy)
        if (absX < SWIPE_DECISION_THRESHOLD && absY < SWIPE_DECISION_THRESHOLD)
          return
        clearHoldTimer()
        cancelPendingTap()
        if (absX >= absY * SWIPE_AXIS_RATIO) {
          gesture.direction = dx < 0 ? 'left' : 'right'
          if (dx < 0 && gesture.swipeAction !== null) {
            previewSwipeAction(gesture.swipeAction, dx)
            pointerEvent.preventDefault()
          }
        } else {
          gesture.direction = 'vertical'
        }
      }}
      onPointerCancel={() => {
        rowRef.current?.classList.remove('touch-gesture-active')
        clearHoldTimer()
        cancelPendingTap()
        gesturePointer.current = null
        settleSwipeAction()
      }}
      onPointerUp={(pointerEvent) => {
        rowRef.current?.classList.remove('touch-gesture-active')
        clearHoldTimer()
        const gesture = gesturePointer.current
        gesturePointer.current = null
        if (
          pointerEvent.pointerType === 'mouse' ||
          gesture === null ||
          gesture.pointerId !== pointerEvent.pointerId
        ) {
          return
        }
        if (gesture.held) {
          pointerEvent.preventDefault()
          return
        }
        if (!touchHoldEnabled && performance.now() - gesture.startedAt >= 550)
          return
        if (gesture.linkOnly) {
          if (
            gesture.direction !== 'none' ||
            Math.hypot(
              pointerEvent.clientX - gesture.startX,
              pointerEvent.clientY - gesture.startY,
            ) > ACTION_ROW_TAP_SLOP_PX
          ) {
            suppressNextClick.current = true
          }
          return
        }
        if (gesture.direction === 'left' && gestureEligible) {
          suppressNextClick.current = true
          settleSwipeAction()
          if (
            swipeDirection(
              { x: gesture.startX, y: gesture.startY },
              pointerEvent.clientX,
              pointerEvent.clientY,
            ) === 'left'
          ) {
            const action = gesture.swipeAction
            if (action !== null) {
              pointerEvent.preventDefault()
              suppressNextClick.current = true
              runGestureAction(action)
            }
          }
          return
        }
        if (
          gesture.direction !== 'none' ||
          Math.hypot(
            pointerEvent.clientX - gesture.startX,
            pointerEvent.clientY - gesture.startY,
          ) > ACTION_ROW_TAP_SLOP_PX
        ) {
          suppressNextClick.current = true
          return
        }
        pointerEvent.preventDefault()
        suppressNextClick.current = true
        finishTap(pointerEvent.clientX, pointerEvent.clientY)
      }}
      onContextMenu={(event) => {
        if (
          touchHoldEnabled &&
          gesturePointer.current !== null &&
          !isTimestampControl(event.target)
        )
          event.preventDefault()
      }}
      onClick={(click) => {
        if (suppressNextClick.current) {
          suppressNextClick.current = false
          click.preventDefault()
          return
        }
        if (!isRowControl(click.target)) {
          if (
            doubleClickAction !== null &&
            lastPointerType.current === 'mouse' &&
            isMessageBodyTarget(click.target)
          ) {
            if (click.detail === 1) {
              cancelDesktopClick()
              desktopClickTimer.current = window.setTimeout(() => {
                desktopClickTimer.current = null
                onOpenActions()
              }, MESSAGE_DOUBLE_TAP_MS)
            }
            return
          }
          onOpenActions()
        }
      }}
      onMouseDown={(mouseDown) => {
        if (
          doubleClickAction !== null &&
          mouseDown.detail === 2 &&
          isMessageBodyTarget(mouseDown.target) &&
          !isRowControl(mouseDown.target)
        ) {
          mouseDown.preventDefault()
        }
      }}
      onDblClick={(click) => {
        if (
          doubleClickAction === null ||
          lastPointerType.current !== 'mouse' ||
          !isMessageBodyTarget(click.target) ||
          isRowControl(click.target)
        )
          return
        cancelDesktopClick()
        click.preventDefault()
        runGestureAction(doubleClickAction)
      }}
    >
      <UserAvatar
        accountId={accountId}
        userId={event.sender}
        displayName={senderDisplay}
        member={members.members.value.get(event.sender)}
      />
      <div class="event-content">
        <div class="event-head">
          <span class="event-sender">{senderDisplay}</span>
          <EventTime
            event={event}
            format={settings.timeFormat.value}
            touchHoldEnabled={touchHoldEnabled}
          />
          <FailedSend event={event} timeline={timeline} />
          {event.edited && (
            <button
              type="button"
              class="ghost edited-marker"
              title="Show edit history"
              onClick={() => setHistoryOpen(true)}
            >
              (edited)
            </button>
          )}
          {(hasMessageActions || developerMode) && (
            <span class="event-actions">
              {hasMessageActions && (
                <>
                  <EventActionButton
                    label="Reply"
                    onClick={() => onReply(event)}
                  >
                    <EventActionIcon name="reply" />
                    <span class="event-action-label">Reply</span>
                  </EventActionButton>
                  {canOpenThread && (
                    <EventActionButton
                      label="Thread"
                      onClick={() => onOpenThread(event.event_id)}
                    >
                      <EventActionIcon name="thread" />
                      <span class="event-action-label">Thread</span>
                    </EventActionButton>
                  )}
                  <EventActionButton
                    label="React"
                    onClick={() =>
                      onSetReactionPicker(
                        reactionPickerOpen ? null : event.event_id,
                      )
                    }
                  >
                    <EventActionIcon name="react" />
                    <span class="event-action-label">React</span>
                  </EventActionButton>
                  {editable && (
                    <EventActionButton
                      label="Edit"
                      onClick={() => onEdit(event)}
                    >
                      <EventActionIcon name="edit" />
                      <span class="event-action-label">Edit</span>
                    </EventActionButton>
                  )}
                  {own &&
                    (confirmingRedact ? (
                      <span class="confirm">
                        <EventActionButton
                          label="Confirm delete"
                          className="danger"
                          onClick={() => {
                            void timeline.redact(event.event_id).then((ok) => {
                              if (ok) onMutation?.()
                              setConfirmingRedact(false)
                            })
                          }}
                        >
                          <EventActionIcon name="confirm" />
                          <span class="event-action-label">Confirm delete</span>
                        </EventActionButton>
                        <EventActionButton
                          label="Cancel"
                          className=""
                          onClick={() => setConfirmingRedact(false)}
                        >
                          <EventActionIcon name="cancel" />
                          <span class="event-action-label">Cancel</span>
                        </EventActionButton>
                      </span>
                    ) : (
                      <EventActionButton
                        label="Delete"
                        className="ghost danger"
                        onClick={() => setConfirmingRedact(true)}
                      >
                        <EventActionIcon name="delete" />
                        <span class="event-action-label">Delete</span>
                      </EventActionButton>
                    ))}
                </>
              )}
              {developerMode && (
                <EventActionButton
                  label={inspectOpen ? 'Hide inspect' : 'Inspect'}
                  ariaExpanded={inspectOpen}
                  onClick={() => setInspectOpen((open) => !open)}
                >
                  <EventActionIcon name="inspect" />
                  <span class="event-action-label">
                    {inspectOpen ? 'Hide inspect' : 'Inspect'}
                  </span>
                </EventActionButton>
              )}
            </span>
          )}
        </div>
        {replyTo !== null && (
          <ReplyContext
            accountId={accountId}
            roomId={event.room_id}
            threadRootId={threadRootId}
            target={timeline.replyTargets.value.get(replyTo)}
            targetId={replyTo}
            members={members}
            onJump={onReplyContextJump}
          />
        )}
        <div class="event-body">
          <EventBody
            event={event}
            senderDisplay={senderDisplay}
            resolveName={members.displayName}
          />
        </div>
        {reactionBurst !== null && (
          <span class="message-reaction-burst" aria-hidden="true">
            {reactionBurst}
          </span>
        )}
        {inspectOpen && <EventInspector event={event} />}
        {(reactionEntries.length > 0 || pendingNewReaction) && (
          <div class="reactions">
            {reactionEntries.map(([emoji, tally]) => {
              const shownTally = presentedReactionTally(
                tally,
                gestureReaction,
                emoji,
                ownUserId,
              )
              const pending =
                gestureReaction?.emoji === emoji
                  ? gestureReaction.removing
                    ? 'removing'
                    : 'adding'
                  : null
              return (
                <ReactionChip
                  key={emoji}
                  emoji={emoji}
                  tally={shownTally}
                  tooltip={reactionTooltip(emoji, shownTally, members)}
                  pending={pending}
                  onToggle={() =>
                    void timeline.toggleReaction(event, emoji).then((ok) => {
                      if (ok) onMutation?.()
                    })
                  }
                />
              )
            })}
            {pendingNewReaction && gestureReaction !== null && (
              <ReactionChip
                key={`pending-${gestureReaction.emoji}`}
                emoji={gestureReaction.emoji}
                tally={{
                  count: 1,
                  me: true,
                  senders: ownUserId === null ? [] : [ownUserId],
                  my_event_ids: [],
                }}
                tooltip={`Adding ${gestureReaction.emoji} reaction`}
                pending="adding"
              />
            )}
          </div>
        )}
        {visibleReadReceipts.length > 0 && (
          <ReadReceiptsSummary
            receipts={visibleReadReceipts}
            members={members}
            accountId={accountId}
          />
        )}
        {reactionPickerOpen && (
          <ReactionPicker
            onClose={() => onSetReactionPicker(null)}
            settings={settings}
            onReact={(key) => {
              onSetReactionPicker(null)
              void timeline.toggleReaction(event, key).then((ok) => {
                if (ok) onMutation?.()
              })
            }}
          />
        )}
        {threadSummary !== undefined && onOpenThread !== undefined && (
          <button
            type="button"
            class={`thread-badge${threadUnread ? ' unread-thread-badge' : ''}`}
            onPointerDown={(pointerEvent) => {
              if (pointerEvent.pointerType === 'mouse') {
                return
              }
              pointerEvent.preventDefault()
              pointerEvent.stopPropagation()
              onOpenThread(event.event_id)
            }}
            onClick={() => onOpenThread(event.event_id)}
          >
            💬 {threadSummary.reply_count}{' '}
            {threadSummary.reply_count === 1 ? 'reply' : 'replies'}
            {threadUnread && <span class="thread-badge-new">New</span>}
          </button>
        )}
        {historyOpen && (
          <EditHistory
            accountId={accountId}
            eventId={event.event_id}
            onClose={() => setHistoryOpen(false)}
          />
        )}
        {gestureFeedback !== null && (
          <span class="message-gesture-feedback" role="status">
            {gestureFeedback}
          </span>
        )}
      </div>
      {swipeReveal !== null && (
        <span class="gesture-swipe-affordance" aria-hidden="true">
          <EventActionIcon name={swipeReveal} />
          <span>{messageGestureActionLabel(swipeReveal)}</span>
        </span>
      )}
    </li>
  )
}

/**
 * "Seen by …" for a row's receipts. Exported so a gallery row can render the
 * same sentence beneath its grid rather than growing a second phrasing.
 */
export function formatReadReceipts(
  receipts: readonly ReadReceipt[],
  members: MembersStore,
): string {
  const names = receipts.map((receipt) => members.displayName(receipt.userId))
  if (names.length === 1) {
    return `Seen by ${names[0]}`
  }
  if (names.length === 2) {
    return `Seen by ${names[0]} and ${names[1]}`
  }
  return `Seen by ${names[0]}, ${names[1]}, and ${names.length - 2} more`
}

/**
 * The "Seen by …" line, expandable to the full list of viewers. Exported so
 * the media gallery row can render the same control beneath its grid rather
 * than growing a second implementation.
 */
export function ReadReceiptsSummary({
  receipts,
  members,
  accountId,
}: {
  receipts: readonly ReadReceipt[]
  members: MembersStore
  accountId: string
}) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLSpanElement>(null)

  useEffect(() => {
    if (!open) {
      return
    }
    const onDismiss = (event: Event) => {
      if (
        event.target instanceof Node &&
        root.current?.contains(event.target)
      ) {
        return
      }
      setOpen(false)
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setOpen(false)
      }
    }
    document.addEventListener('mousedown', onDismiss)
    document.addEventListener('touchstart', onDismiss)
    document.addEventListener('keydown', onKeyDown)
    document.addEventListener('scroll', onDismiss, true)
    return () => {
      document.removeEventListener('mousedown', onDismiss)
      document.removeEventListener('touchstart', onDismiss)
      document.removeEventListener('keydown', onKeyDown)
      document.removeEventListener('scroll', onDismiss, true)
    }
  }, [open])

  return (
    <span class="read-receipts-wrap" ref={root}>
      <button
        type="button"
        class="read-receipts"
        aria-expanded={open}
        onClick={() => setOpen((was) => !was)}
      >
        {formatReadReceipts(receipts, members)}
      </button>
      {open && (
        // A static, non-selectable list of names: native `ul`/`li` with a
        // label, not `role="listbox"`, which would promise `option` roles,
        // `aria-selected` and arrow-key navigation that none of this has.
        <ul class="read-receipts-popover" aria-label="Seen by">
          {receipts.map((receipt) => (
            <li key={receipt.userId} class="read-receipts-row">
              <UserAvatar
                accountId={accountId}
                userId={receipt.userId}
                displayName={members.displayName(receipt.userId)}
                member={members.members.value.get(receipt.userId)}
              />
              <span class="read-receipts-name">
                {members.displayName(receipt.userId)}
              </span>
            </li>
          ))}
        </ul>
      )}
    </span>
  )
}

function eventDiagnostics(event: TimelineEvent) {
  return {
    account_id: event.account_id,
    event_id: event.event_id,
    room_id: event.room_id,
    sender: event.sender,
    origin_ts: event.origin_ts,
    arrival_order: event.arrival_order,
    type: event.type,
    state_key: event.state_key ?? null,
    body: event.body ?? null,
    content: event.content ?? null,
    redacted: event.redacted,
    redaction_event_id: event.redaction_event_id ?? null,
    relates_to: event.relates_to ?? null,
    sender_trust: event.sender_trust ?? null,
    edited: event.edited,
    edit_count: event.edit_count,
    latest_edit_ts: event.latest_edit_ts ?? null,
    reactions: event.reactions ?? null,
  }
}

function EventInspector({ event }: { event: TimelineEvent }) {
  const json = JSON.stringify(eventDiagnostics(event), null, 2)
  const { status, copy } = useCopyFeedback()
  const title =
    status === 'copied'
      ? 'Copied'
      : status === 'failed'
        ? 'Could not copy'
        : 'Copy API event data'
  return (
    <section
      class="event-inspector"
      aria-label={`Event diagnostics for ${event.event_id}`}
    >
      <div class="event-inspector-title">
        <span>API event data</span>
        <span class="event-inspector-copy-cluster">
          {status !== 'idle' && (
            <span
              class={`event-copy-status${status === 'failed' ? ' error' : ''}`}
              role="status"
            >
              {status === 'copied' ? 'Copied' : 'Copy failed'}
            </span>
          )}
          <button
            type="button"
            class="ghost event-inspector-copy"
            title={title}
            aria-label="Copy API event data"
            onClick={() => void copy(json)}
          >
            <EventActionIcon name={status === 'copied' ? 'confirm' : 'copy'} />
          </button>
        </span>
      </div>
      <pre>{json}</pre>
    </section>
  )
}

export function ReactionPicker({
  onClose,
  settings,
  onReact,
  ariaLabel = 'React with',
}: {
  onClose: () => void
  settings: SettingsStore
  onReact: (key: string) => void
  ariaLabel?: string
}) {
  const { containerRef } = useModalFocus<HTMLDivElement>()
  const fullPickerDialogRef = useRef<HTMLDivElement>(null)
  const fullPickerHostRef = useRef<HTMLDivElement>(null)
  const moreButtonRef = useRef<HTMLButtonElement>(null)
  const [mode, setMode] = useState<'compact' | 'full'>('compact')
  const [anchorRect, setAnchorRect] = useState<DOMRectReadOnly | null>(null)
  const [pickerPosition, setPickerPosition] = useState<PickerPosition | null>(
    null,
  )
  const recentReactions = settings.recentReactions.value
  const quickReactions = [
    ...QUICK_REACTIONS,
    ...recentReactions.filter((key) => !QUICK_REACTIONS.includes(key)),
  ]

  const openFullPicker = useCallback((anchor: HTMLElement | null) => {
    setAnchorRect(anchor?.getBoundingClientRect() ?? null)
    setPickerPosition(null)
    setMode('full')
  }, [])

  useShortcuts(
    {
      '+': (event) => {
        if (mode !== 'compact') {
          return
        }
        event.preventDefault()
        openFullPicker(moreButtonRef.current)
      },
      Escape: (event) => {
        event.preventDefault()
        onClose()
      },
    },
    { whileTyping: true, capture: true },
  )

  const react = useCallback(
    (key: string) => {
      const trimmed = canonicalReactionKey(key.trim())
      if (trimmed === '') {
        return
      }
      settings.recordRecentReaction(trimmed)
      onReact(trimmed)
    },
    [onReact, settings],
  )
  const reactRef = useRef(react)

  useEffect(() => {
    reactRef.current = react
  }, [react])

  useEffect(() => {
    containerRef.current?.scrollIntoView?.({
      block: 'nearest',
      inline: 'nearest',
    })
  }, [containerRef])

  useLayoutEffect(() => {
    if (mode !== 'full') {
      return
    }
    const dialog = fullPickerDialogRef.current
    if (dialog === null) {
      return
    }
    document.body.append(dialog)
    return () => dialog.remove()
  }, [mode])

  const positionFullPicker = useCallback(() => {
    if (mode !== 'full') {
      return
    }
    const dialog = fullPickerDialogRef.current
    if (dialog === null) {
      return
    }
    const dialogBox = dialog.getBoundingClientRect()
    const viewportWidth = window.innerWidth
    const viewportHeight = window.innerHeight
    const fallbackAnchor = {
      top: viewportHeight / 2,
      right: viewportWidth / 2,
      bottom: viewportHeight / 2,
    }
    const anchor = anchorRect ?? fallbackAnchor

    setPickerPosition(
      fullReactionPickerPosition({
        anchor,
        dialog: dialogBox,
        viewport: { width: viewportWidth, height: viewportHeight },
      }),
    )
  }, [anchorRect, mode])

  useLayoutEffect(() => {
    positionFullPicker()
  }, [positionFullPicker])

  useEffect(() => {
    if (mode !== 'full') {
      return
    }
    window.addEventListener('resize', positionFullPicker)
    window.addEventListener('scroll', positionFullPicker, true)
    return () => {
      window.removeEventListener('resize', positionFullPicker)
      window.removeEventListener('scroll', positionFullPicker, true)
    }
  }, [mode, positionFullPicker])

  const setFullPickerDialogRef = useCallback(
    (element: HTMLDivElement | null) => {
      fullPickerDialogRef.current = element
      containerRef.current = element
    },
    [containerRef],
  )

  useEffect(() => {
    if (mode !== 'full') {
      return
    }
    const host = fullPickerHostRef.current
    if (host === null) {
      return
    }

    let cancelled = false
    let picker: HTMLElement | null = null
    let teardownFocusVisible = () => {}
    const onEmojiClick = (event: Event) => {
      const detail = (event as CustomEvent<EmojiPickerClickDetail>).detail
      reactRef.current(detail.unicode ?? detail.emoji?.unicode ?? '')
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (
        event.key !== 'ArrowLeft' &&
        event.key !== 'ArrowRight' &&
        event.key !== 'ArrowUp' &&
        event.key !== 'ArrowDown'
      ) {
        return
      }
      if (picker !== null) {
        moveEmojiPickerFocus(picker, event)
      }
    }
    const mountPicker = (nextPicker: HTMLElement) => {
      if (cancelled) {
        return
      }
      picker = nextPicker
      teardownFocusVisible = installEmojiPickerFocusVisible(picker)
      picker.addEventListener('emoji-click', onEmojiClick)
      picker.addEventListener('keydown', onKeyDown)
      host.replaceChildren(picker)
      focusEmojiPickerSearch(picker)
    }

    void import('emoji-picker-element').then(({ Picker }) =>
      mountPicker(new Picker({ dataSource: EMOJI_PICKER_DATA_SOURCE })),
    )

    return () => {
      cancelled = true
      teardownFocusVisible()
      picker?.removeEventListener('emoji-click', onEmojiClick)
      picker?.removeEventListener('keydown', onKeyDown)
      picker?.remove()
    }
  }, [mode])

  const fullPickerDialog =
    mode === 'full' ? (
      <div
        ref={setFullPickerDialogRef}
        class="reaction-full-picker"
        style={
          pickerPosition === null
            ? undefined
            : {
                left: `${pickerPosition.left}px`,
                top: `${pickerPosition.top}px`,
              }
        }
        role="dialog"
        aria-label="Emoji picker"
      >
        <div class="reaction-full-picker-head">
          <span>Emoji picker</span>
          <button type="button" class="ghost" onClick={onClose}>
            Close picker
          </button>
        </div>
        <div ref={fullPickerHostRef} class="reaction-full-picker-host" />
      </div>
    ) : null

  return (
    <div
      ref={mode === 'compact' ? containerRef : null}
      class="reaction-picker-shell"
      role="group"
      aria-label={ariaLabel}
    >
      <div class="reaction-picker">
        {quickReactions.map((key) => (
          <button key={key} type="button" onClick={() => react(key)}>
            {key}
          </button>
        ))}
        {mode === 'compact' ? (
          <button
            ref={moreButtonRef}
            type="button"
            class="reaction-more"
            aria-label="More reactions"
            onClick={(event) => openFullPicker(event.currentTarget)}
          >
            +
          </button>
        ) : null}
      </div>
      {fullPickerDialog}
    </div>
  )
}

function focusEmojiPickerSearch(picker: HTMLElement): void {
  const focusSearch = () => {
    const root = picker.shadowRoot ?? picker
    const search = root.querySelector<HTMLInputElement>(
      'input[type="search"], input[aria-label*="search" i], input',
    )
    if (search !== null) {
      search.focus()
      return true
    }
    return false
  }

  if (focusSearch()) {
    return
  }
  requestAnimationFrame(() => {
    if (focusSearch()) {
      return
    }
    window.setTimeout(focusSearch, 0)
  })
}

function installEmojiPickerFocusVisible(picker: HTMLElement): () => void {
  const root = picker.shadowRoot
  if (root === null) {
    return () => {}
  }

  picker.setAttribute('data-js-focus-visible', '')
  const style = document.createElement('style')
  style.textContent = `
    [data-focus-visible-added] {
      outline: 2px solid var(--outline-color) !important;
      outline-offset: -2px !important;
    }
    button[data-focus-visible-added],
    .nav-button[data-focus-visible-added],
    .emoji[data-focus-visible-added] {
      background: var(--button-hover-background) !important;
      box-shadow: inset 0 0 0 2px var(--outline-color) !important;
    }
    input[data-focus-visible-added] {
      box-shadow: inset 0 0 0 2px var(--outline-color) !important;
    }
  `
  root.append(style)
  let keyboardMode = false

  const clear = () => {
    for (const element of root.querySelectorAll('[data-focus-visible-added]')) {
      element.removeAttribute('data-focus-visible-added')
    }
  }

  const mark = (element: HTMLElement | null) => {
    if (!keyboardMode || element === null) {
      return
    }
    clear()
    element.setAttribute('data-focus-visible-added', '')
  }

  const onKeyDown = () => {
    keyboardMode = true
    queueMicrotask(() => {
      mark(
        root.activeElement instanceof HTMLElement ? root.activeElement : null,
      )
    })
  }
  const onPointerDown = () => {
    keyboardMode = false
    clear()
  }
  const onFocusIn = (event: Event) => {
    mark(event.target instanceof HTMLElement ? event.target : null)
  }

  document.addEventListener('keydown', onKeyDown, true)
  root.addEventListener('pointerdown', onPointerDown, true)
  root.addEventListener('mousedown', onPointerDown, true)
  root.addEventListener('focusin', onFocusIn)

  return () => {
    document.removeEventListener('keydown', onKeyDown, true)
    root.removeEventListener('pointerdown', onPointerDown, true)
    root.removeEventListener('mousedown', onPointerDown, true)
    root.removeEventListener('focusin', onFocusIn)
    clear()
    style.remove()
    picker.removeAttribute('data-js-focus-visible')
  }
}

function moveEmojiPickerFocus(
  picker: HTMLElement,
  event: KeyboardEvent,
): boolean {
  const root = picker.shadowRoot
  if (root === null) {
    return false
  }
  const active = root.activeElement
  if (!(active instanceof HTMLElement)) {
    return false
  }

  const emojiButton = active.closest('button[role="menuitem"]')
  if (emojiButton instanceof HTMLButtonElement) {
    const menu = emojiButton.closest('.emoji-menu')
    if (!(menu instanceof HTMLElement)) {
      return false
    }
    const buttons = [
      ...menu.querySelectorAll<HTMLButtonElement>('button[role="menuitem"]'),
    ]
    const index = buttons.indexOf(emojiButton)
    if (index === -1) {
      return false
    }
    const columns = Math.max(
      getComputedStyle(menu)
        .gridTemplateColumns.split(' ')
        .filter((value) => value !== '').length,
      1,
    )
    const step =
      event.key === 'ArrowLeft'
        ? -1
        : event.key === 'ArrowRight'
          ? 1
          : event.key === 'ArrowUp'
            ? -columns
            : event.key === 'ArrowDown'
              ? columns
              : 0
    if (step === 0) {
      return false
    }
    const next = buttons[index + step]
    if (next === undefined) {
      return false
    }
    event.preventDefault()
    next.focus()
    return true
  }

  const navButton = active.closest('.nav-button')
  if (
    navButton instanceof HTMLButtonElement &&
    (event.key === 'ArrowLeft' || event.key === 'ArrowRight')
  ) {
    const buttons = [...root.querySelectorAll<HTMLButtonElement>('.nav-button')]
    const index = buttons.indexOf(navButton)
    const next =
      event.key === 'ArrowLeft' ? buttons[index - 1] : buttons[index + 1]
    if (next === undefined) {
      return false
    }
    event.preventDefault()
    next.focus()
    return true
  }

  return false
}

export function canonicalReactionKey(key: string): string {
  const withoutEmojiPresentation = key.replaceAll('\uFE0F', '')
  return QUICK_REACTIONS.includes(withoutEmojiPresentation)
    ? withoutEmojiPresentation
    : key
}

function ReactionChip({
  emoji,
  tally,
  tooltip,
  pending = null,
  onToggle,
}: {
  emoji: string
  tally: ReactionTally
  tooltip: string
  pending?: 'adding' | 'removing' | null
  onToggle?: () => void
}) {
  const [tooltipOpen, setTooltipOpen] = useState(false)
  const root = useRef<HTMLSpanElement>(null)
  const longPressTimer = useRef<number | null>(null)
  const suppressNextClick = useRef(false)
  const tooltipId = `reaction-tooltip-${encodeURIComponent(emoji)}-${tally.count}`

  const clearLongPress = () => {
    if (longPressTimer.current !== null) {
      window.clearTimeout(longPressTimer.current)
      longPressTimer.current = null
    }
  }

  useEffect(() => {
    if (!tooltipOpen) {
      return
    }
    const onDismiss = (event: Event) => {
      if (
        event.target instanceof Node &&
        root.current?.contains(event.target)
      ) {
        return
      }
      setTooltipOpen(false)
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setTooltipOpen(false)
      }
    }
    document.addEventListener('mousedown', onDismiss)
    document.addEventListener('touchstart', onDismiss)
    document.addEventListener('keydown', onKeyDown)
    document.addEventListener('scroll', onDismiss, true)
    return () => {
      document.removeEventListener('mousedown', onDismiss)
      document.removeEventListener('touchstart', onDismiss)
      document.removeEventListener('keydown', onKeyDown)
      document.removeEventListener('scroll', onDismiss, true)
    }
  }, [tooltipOpen])

  useEffect(
    () => () => {
      if (longPressTimer.current !== null) {
        window.clearTimeout(longPressTimer.current)
      }
    },
    [],
  )

  return (
    <span
      ref={root}
      class="reaction-chip-wrap"
      onMouseEnter={() => setTooltipOpen(true)}
      onMouseLeave={() => setTooltipOpen(false)}
    >
      <button
        type="button"
        class={`reaction-chip${tally.me ? ' mine' : ''}${pending === null ? '' : ` gesture-reaction-pending gesture-reaction-${pending}`}`}
        aria-busy={pending === null ? undefined : 'true'}
        aria-describedby={tooltipOpen ? tooltipId : undefined}
        disabled={pending !== null}
        onFocus={() => setTooltipOpen(true)}
        onBlur={() => setTooltipOpen(false)}
        onTouchStart={() => {
          clearLongPress()
          // Each touch re-arms from scratch: a long press that never produced a
          // click (the user lifted and tapped elsewhere) must not leave the flag
          // set to swallow the *next* genuine tap on this chip.
          suppressNextClick.current = false
          longPressTimer.current = window.setTimeout(() => {
            suppressNextClick.current = true
            setTooltipOpen(true)
          }, REACTION_TOUCH_HOLD_MS)
        }}
        onTouchMove={clearLongPress}
        onTouchEnd={clearLongPress}
        onTouchCancel={clearLongPress}
        onClick={(event) => {
          if (suppressNextClick.current) {
            event.preventDefault()
            suppressNextClick.current = false
            return
          }
          onToggle?.()
        }}
      >
        {emoji} {tally.count}
      </button>
      {tooltipOpen && (
        <span id={tooltipId} class="reaction-senders-tooltip" role="tooltip">
          {tooltip}
        </span>
      )}
    </span>
  )
}

function reactionTooltip(
  emoji: string,
  tally: ReactionTally,
  members: MembersStore,
): string {
  const names = tally.senders.map((sender) => members.displayName(sender))
  if (names.length === 0) {
    return emoji
  }
  const shown = names.slice(0, REACTION_TOOLTIP_NAME_LIMIT)
  const remaining = names.length - shown.length
  return remaining === 0
    ? `${emoji}: ${shown.join(', ')}`
    : `${emoji}: ${shown.join(', ')}, and ${remaining} more`
}

/** The quoted context above a rich reply (ADR 0033 display). */
function ReplyContext({
  accountId,
  roomId,
  threadRootId,
  target,
  targetId,
  members,
  onJump,
}: {
  accountId: string
  roomId: string
  threadRootId: string | undefined
  target: EventDto | undefined
  targetId: string
  members: MembersStore
  onJump: ((eventId: string) => void) | undefined
}) {
  const href =
    threadRootId === undefined
      ? localRoomHref(accountId, roomId, targetId)
      : localThreadEventHref(accountId, roomId, threadRootId, targetId)
  if (target === undefined) {
    return (
      <blockquote class="reply-context muted">
        <a
          class="reply-context-link"
          href={href}
          aria-label="Jump to original message"
          onClick={(event) => {
            if (
              onJump !== undefined &&
              event.button === 0 &&
              !event.metaKey &&
              !event.ctrlKey &&
              !event.altKey &&
              !event.shiftKey
            ) {
              event.preventDefault()
              onJump(targetId)
            }
          }}
        />
        in reply to <code>{targetId}</code>
      </blockquote>
    )
  }
  const senderDisplay = members.displayName(target.sender)
  return (
    <blockquote class="reply-context">
      <a
        class="reply-context-link"
        href={href}
        aria-label={`Jump to original message from ${senderDisplay}`}
        onClick={(event) => {
          if (
            onJump !== undefined &&
            event.button === 0 &&
            !event.metaKey &&
            !event.ctrlKey &&
            !event.altKey &&
            !event.shiftKey
          ) {
            event.preventDefault()
            onJump(targetId)
          }
        }}
      />
      <UserAvatar
        accountId={accountId}
        userId={target.sender}
        displayName={senderDisplay}
        member={members.members.value.get(target.sender)}
      />
      <span class="reply-context-copy">
        <span class="event-sender">{senderDisplay}</span>{' '}
        <span class="muted reply-context-body">
          <ReplyContextBody event={target} senderDisplay={senderDisplay} />
        </span>
      </span>
    </blockquote>
  )
}

function ReplyContextBody({
  event,
  senderDisplay,
}: {
  event: EventDto
  senderDisplay: string
}) {
  const formattedBody = (
    event.content as { formatted_body?: unknown } | null | undefined
  )?.formatted_body
  if (typeof formattedBody === 'string') {
    return <EventBody event={event} senderDisplay={senderDisplay} />
  }
  const excerpt = event.body ?? (event.redacted ? 'message deleted' : '…')
  return <>{clip(excerpt, 200)}</>
}

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}...` : text
}
