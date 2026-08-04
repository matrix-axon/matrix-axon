import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from 'preact/hooks'
import type { ComponentChildren } from 'preact'
import { EMOJI_PICKER_DATA_SOURCE } from '../emoji'
import { parseMedia } from '../media/parse-media'
import { localRoomHref, localThreadEventHref } from '../matrix-to'
import { useShortcuts } from '../shortcuts'
import type { SettingsStore } from '../stores/settings'
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
  reactionPickerOpen,
  onSetReactionPicker,
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
  reactionPickerOpen: boolean
  onSetReactionPicker: (eventId: string | null) => void
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
  const [historyOpen, setHistoryOpen] = useState(false)
  const [confirmingRedact, setConfirmingRedact] = useState(false)
  const [inspectOpen, setInspectOpen] = useState(false)
  const [actionsOpen, setActionsOpen] = useState(false)
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
  const visibleReadReceipts = readReceipts.filter(
    (receipt) =>
      receipt.userId !== ownUserId && receipt.userId !== event.sender,
  )

  return (
    <li
      class={`event-row${isState ? ' state-event' : ''}${highlighted ? ' highlighted' : ''}${pending ? ' pending' : ''}${failed ? ' failed' : ''}${actionsOpen ? ' actions-open' : ''}`}
      data-event-id={event.event_id}
      onClick={(click) => {
        if (!isRowControl(click.target)) {
          setActionsOpen(true)
        }
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
          <EventTime event={event} format={settings.timeFormat.value} />
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
        {inspectOpen && <EventInspector event={event} />}
        {event.reactions != null && Object.keys(event.reactions).length > 0 && (
          <div class="reactions">
            {Object.entries(event.reactions).map(([emoji, tally]) => (
              <ReactionChip
                key={emoji}
                emoji={emoji}
                tally={tally}
                tooltip={reactionTooltip(emoji, tally, members)}
                onToggle={() =>
                  void timeline.toggleReaction(event, emoji).then((ok) => {
                    if (ok) onMutation?.()
                  })
                }
              />
            ))}
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
      </div>
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
  return (
    <section
      class="event-inspector"
      aria-label={`Event diagnostics for ${event.event_id}`}
    >
      <div class="event-inspector-title">API event data</div>
      <pre>{JSON.stringify(eventDiagnostics(event), null, 2)}</pre>
    </section>
  )
}

function ReactionPicker({
  onClose,
  settings,
  onReact,
}: {
  onClose: () => void
  settings: SettingsStore
  onReact: (key: string) => void
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
      aria-label="React with"
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

type ReactionTally = NonNullable<EventDto['reactions']>[string]

function ReactionChip({
  emoji,
  tally,
  tooltip,
  onToggle,
}: {
  emoji: string
  tally: ReactionTally
  tooltip: string
  onToggle: () => void
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
        class={`reaction-chip${tally.me ? ' mine' : ''}`}
        aria-describedby={tooltipOpen ? tooltipId : undefined}
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
          onToggle()
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
