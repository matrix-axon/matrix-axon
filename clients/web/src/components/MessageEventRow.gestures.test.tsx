import { signal } from '@preact/signals'
import { act, cleanup, fireEvent, render } from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { ServicesContext } from '../services'
import type { MembersStore } from '../stores/members'
import {
  defaultMessageGestures,
  type MessageGesturePreferences,
} from '../stores/message-gestures'
import type { TimelineEvent, TimelineStore } from '../stores/timeline'
import { testServices } from '../test/services'
import { MessageEventRow } from './MessageEventRow'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const OWN_USER = '@me:hs'

function event(overrides: Partial<TimelineEvent> = {}): TimelineEvent {
  return {
    account_id: ACCOUNT,
    event_id: '$event',
    room_id: '!room:hs',
    sender: OWN_USER,
    origin_ts: 1_000,
    arrival_order: 1_000,
    type: 'm.room.message',
    body: 'hello',
    content: { msgtype: 'm.text', body: 'hello' },
    redacted: false,
    edited: false,
    edit_count: 0,
    state_key: null,
    reactions: null,
    relates_to: null,
    ...overrides,
  } as TimelineEvent
}

const members = {
  displayName: (userId: string) => userId,
  members: signal(new Map()),
} as unknown as MembersStore

function renderRow({
  preferences = defaultMessageGestures(),
  rowEvent = event(),
  ownUserId = OWN_USER,
  showThreadAction = true,
}: {
  preferences?: MessageGesturePreferences | null
  rowEvent?: TimelineEvent
  ownUserId?: string | null
  showThreadAction?: boolean
} = {}) {
  const services = testServices()
  const timeline = {
    toggleReaction: vi.fn().mockResolvedValue(true),
    redact: vi.fn().mockResolvedValue(true),
    replyTargets: signal(new Map()),
  } as unknown as TimelineStore
  const onOpenActions = vi.fn()
  const onReply = vi.fn()
  const onEdit = vi.fn()
  const onOpenThread = vi.fn()
  const view = render(
    <ServicesContext.Provider value={services}>
      <ol>
        <MessageEventRow
          event={rowEvent}
          timeline={timeline}
          members={members}
          accountId={ACCOUNT}
          ownUserId={ownUserId}
          settings={services.settings}
          messageGestures={preferences}
          reactionPickerOpen={false}
          onSetReactionPicker={() => {}}
          actionsOpen={false}
          onOpenActions={onOpenActions}
          onReply={onReply}
          onEdit={onEdit}
          onOpenThread={onOpenThread}
          showThreadAction={showThreadAction}
        />
      </ol>
    </ServicesContext.Provider>,
  )
  const row = view.container.querySelector('li.event-row') as HTMLElement
  const body = row.querySelector('.event-body')!
  return {
    ...view,
    row,
    body,
    timeline,
    onOpenActions,
    onReply,
    onEdit,
    onOpenThread,
  }
}

function pointer(
  target: Element,
  kind: 'down' | 'move' | 'up',
  x: number,
  y = 40,
) {
  const init = {
    pointerType: 'touch',
    pointerId: 1,
    clientX: x,
    clientY: y,
  }
  if (kind === 'down') fireEvent.pointerDown(target, init)
  else if (kind === 'move') fireEvent.pointerMove(target, init)
  else fireEvent.pointerUp(target, init)
}

afterEach(() => {
  cleanup()
  vi.useRealTimers()
})

describe('MessageEventRow gestures', () => {
  it('toggles the configured emoji on double tap and does not open actions', async () => {
    vi.useFakeTimers()
    const view = renderRow()

    pointer(view.body, 'down', 40)
    pointer(view.body, 'up', 40)
    pointer(view.body, 'down', 41)
    pointer(view.body, 'up', 41)
    await act(async () => await Promise.resolve())
    vi.advanceTimersByTime(300)

    expect(view.timeline.toggleReaction).toHaveBeenCalledWith(
      expect.objectContaining({ event_id: '$event' }),
      '👍',
    )
    expect(view.onOpenActions).not.toHaveBeenCalled()
  })

  it('runs the configured double-tap action and claims native word selection', () => {
    vi.useFakeTimers()
    const preferences: MessageGesturePreferences = {
      schema_version: 1,
      bindings: {
        double_tap: 'reply',
        touch_and_hold: 'thread',
        swipe_left: 'react',
      },
      reaction_emoji: '👍',
    }
    const view = renderRow({ preferences })

    fireEvent.pointerDown(view.body, { pointerType: 'mouse', pointerId: 1 })
    fireEvent.click(view.body, { detail: 1 })
    fireEvent.pointerDown(view.body, { pointerType: 'mouse', pointerId: 1 })
    const secondMouseDown = new MouseEvent('mousedown', {
      bubbles: true,
      cancelable: true,
      detail: 2,
    })
    view.body.dispatchEvent(secondMouseDown)
    fireEvent.click(view.body, { detail: 2 })
    const doubleClick = new MouseEvent('dblclick', {
      bubbles: true,
      cancelable: true,
      detail: 2,
    })
    view.body.dispatchEvent(doubleClick)

    expect(view.onReply).toHaveBeenCalledWith(
      expect.objectContaining({ event_id: '$event' }),
    )
    expect(view.onOpenActions).not.toHaveBeenCalled()
    expect(secondMouseDown.defaultPrevented).toBe(true)
    expect(doubleClick.defaultPrevented).toBe(true)
    act(() => {
      vi.advanceTimersByTime(300)
    })
    expect(view.onOpenActions).not.toHaveBeenCalled()
  })

  it('still opens actions after a single mouse click settles', () => {
    vi.useFakeTimers()
    const view = renderRow()

    fireEvent.pointerDown(view.body, { pointerType: 'mouse', pointerId: 1 })
    fireEvent.click(view.body, { detail: 1 })
    expect(view.onOpenActions).not.toHaveBeenCalled()

    act(() => {
      vi.advanceTimersByTime(300)
    })
    expect(view.onOpenActions).toHaveBeenCalledOnce()
  })

  it('shows an emoji burst and optimistic chip while a reaction is pending', async () => {
    vi.useFakeTimers()
    let finishReaction!: (ok: boolean) => void
    const view = renderRow()
    view.timeline.toggleReaction = vi.fn(
      () =>
        new Promise<boolean>((resolve) => {
          finishReaction = resolve
        }),
    )

    pointer(view.body, 'down', 40)
    pointer(view.body, 'up', 40)
    pointer(view.body, 'down', 41)
    pointer(view.body, 'up', 41)

    expect(view.row.querySelector('.message-reaction-burst')?.textContent).toBe(
      '👍',
    )
    const pending = view.row.querySelector(
      '.reaction-chip.gesture-reaction-pending',
    ) as HTMLButtonElement
    expect(pending.textContent).toContain('👍 1')
    expect(pending.classList.contains('gesture-reaction-adding')).toBe(true)
    expect(pending.disabled).toBe(true)

    await act(async () => finishReaction(true))
    expect(
      view.row.querySelector('.reaction-chip.gesture-reaction-pending'),
    ).toBeNull()

    act(() => {
      vi.advanceTimersByTime(450)
    })
    expect(view.row.querySelector('.message-reaction-burst')).toBeNull()
  })

  it('dims an existing reaction while removing it and reports failure', async () => {
    vi.useFakeTimers()
    let finishReaction!: (ok: boolean) => void
    const view = renderRow({
      rowEvent: event({
        reactions: {
          '👍': {
            count: 1,
            me: true,
            senders: [OWN_USER],
            my_event_ids: ['$reaction'],
          },
        },
      }),
    })
    view.timeline.toggleReaction = vi.fn(
      () =>
        new Promise<boolean>((resolve) => {
          finishReaction = resolve
        }),
    )

    pointer(view.body, 'down', 40)
    pointer(view.body, 'up', 40)
    pointer(view.body, 'down', 41)
    pointer(view.body, 'up', 41)

    expect(view.row.querySelector('.message-reaction-burst')).toBeNull()
    const pending = view.row.querySelector(
      '.reaction-chip.gesture-reaction-pending',
    ) as HTMLButtonElement
    expect(pending.classList.contains('gesture-reaction-removing')).toBe(true)
    expect(pending.disabled).toBe(true)

    await act(async () => finishReaction(false))
    expect(view.getByRole('status').textContent).toBe(
      'Reaction could not be updated',
    )
    expect(
      view.row.querySelector('.reaction-chip.gesture-reaction-pending'),
    ).toBeNull()
  })

  it('opens the thread on touch and hold', () => {
    vi.useFakeTimers()
    const view = renderRow()

    pointer(view.body, 'down', 40)
    act(() => {
      vi.advanceTimersByTime(550)
    })

    expect(view.onOpenThread).toHaveBeenCalledWith('$event')
    expect(view.row.classList.contains('touch-hold-enabled')).toBe(true)
  })

  it('limits selection and context-menu suppression to an active touch', () => {
    const view = renderRow()
    const desktopContextMenu = new MouseEvent('contextmenu', {
      bubbles: true,
      cancelable: true,
    })

    view.body.dispatchEvent(desktopContextMenu)

    expect(desktopContextMenu.defaultPrevented).toBe(false)
    expect(view.row.classList.contains('touch-gesture-active')).toBe(false)

    pointer(view.body, 'down', 40)
    const touchContextMenu = new MouseEvent('contextmenu', {
      bubbles: true,
      cancelable: true,
    })
    view.body.dispatchEvent(touchContextMenu)

    expect(touchContextMenu.defaultPrevented).toBe(true)
    expect(view.row.classList.contains('touch-gesture-active')).toBe(true)

    pointer(view.body, 'up', 40)
    expect(view.row.classList.contains('touch-gesture-active')).toBe(false)
  })

  it('does not open the delayed action bar under a second-tap hold', () => {
    vi.useFakeTimers()
    const view = renderRow()

    pointer(view.body, 'down', 40)
    pointer(view.body, 'up', 40)
    pointer(view.body, 'down', 41)
    act(() => {
      vi.advanceTimersByTime(550)
    })

    expect(view.onOpenThread).toHaveBeenCalledWith('$event')
    expect(view.onOpenActions).not.toHaveBeenCalled()
  })

  it('allows a normal inline-link tap while reserving link hold', () => {
    const rowEvent = event({
      body: 'https://example.com',
      content: {
        msgtype: 'm.text',
        body: 'https://example.com',
      } as unknown as TimelineEvent['content'],
    })
    const view = renderRow({ rowEvent })
    const link = view.row.querySelector('a')!

    pointer(link, 'down', 40)
    pointer(link, 'up', 40)

    let preventedByRow = true
    view.row.parentElement!.addEventListener(
      'click',
      (click) => {
        preventedByRow = click.defaultPrevented
        click.preventDefault()
      },
      { once: true },
    )
    fireEvent.click(link)

    expect(preventedByRow).toBe(false)
    expect(view.onOpenActions).not.toHaveBeenCalled()
    expect(view.onOpenThread).not.toHaveBeenCalled()
  })

  it('tracks a swipe left, signals the threshold, and replies on release', () => {
    const view = renderRow()

    pointer(view.body, 'down', 130)
    pointer(view.body, 'move', 90)

    expect(view.row.classList.contains('gesture-swipe-reveal')).toBe(true)
    expect(view.row.style.getPropertyValue('--message-swipe-offset')).toBe(
      '-40px',
    )
    expect(view.row.classList.contains('gesture-swipe-armed')).toBe(false)
    expect(
      view.row.querySelector('.gesture-swipe-affordance')?.textContent,
    ).toContain('Reply')

    pointer(view.body, 'move', 45)

    expect(view.row.classList.contains('gesture-swipe-armed')).toBe(true)
    pointer(view.body, 'up', 45)

    expect(view.onReply).toHaveBeenCalledWith(
      expect.objectContaining({ event_id: '$event' }),
    )
    expect(view.row.classList.contains('gesture-swipe-settling')).toBe(true)
    expect(view.row.style.getPropertyValue('--message-swipe-offset')).toBe(
      '0px',
    )
  })

  it('yields a left swipe to horizontally scrollable message content', () => {
    const view = renderRow()
    const scroller = document.createElement('div')
    scroller.style.overflowX = 'auto'
    Object.defineProperties(scroller, {
      scrollWidth: { value: 400, configurable: true },
      clientWidth: { value: 200, configurable: true },
    })
    view.body.append(scroller)

    pointer(scroller, 'down', 130)
    pointer(scroller, 'move', 45)
    pointer(scroller, 'up', 45)

    expect(view.onReply).not.toHaveBeenCalled()
    expect(view.row.classList.contains('gesture-swipe-reveal')).toBe(false)
  })

  it('settles without acting when a swipe left stops short of the threshold', () => {
    const view = renderRow()

    pointer(view.body, 'down', 130)
    pointer(view.body, 'move', 90)
    pointer(view.body, 'up', 90)

    expect(view.onReply).not.toHaveBeenCalled()
    expect(view.row.classList.contains('gesture-swipe-settling')).toBe(true)
    expect(view.row.style.getPropertyValue('--message-swipe-offset')).toBe(
      '0px',
    )
  })

  it('does not claim swipe right for a row action or tap', () => {
    const view = renderRow()

    pointer(view.body, 'down', 40)
    pointer(view.body, 'move', 130)
    pointer(view.body, 'up', 130)

    expect(view.onReply).not.toHaveBeenCalled()
    expect(view.onOpenActions).not.toHaveBeenCalled()
    expect(view.timeline.toggleReaction).not.toHaveBeenCalled()
  })

  it('shows a row-local status when a mapped action is unavailable', () => {
    const preferences: MessageGesturePreferences = {
      schema_version: 1,
      bindings: {
        double_tap: 'react',
        touch_and_hold: 'thread',
        swipe_left: 'edit',
      },
      reaction_emoji: '👍',
    }
    const view = renderRow({
      preferences,
      rowEvent: event({ sender: '@alice:hs' }),
      ownUserId: OWN_USER,
    })

    pointer(view.body, 'down', 130)
    pointer(view.body, 'move', 45)
    pointer(view.body, 'up', 45)

    expect(view.getByRole('status').textContent).toBe(
      'Edit is unavailable for this message',
    )
    expect(view.onEdit).not.toHaveBeenCalled()
  })

  it('opens delete confirmation instead of redacting immediately', () => {
    const preferences: MessageGesturePreferences = {
      schema_version: 1,
      bindings: {
        double_tap: 'react',
        touch_and_hold: 'thread',
        swipe_left: 'delete',
      },
      reaction_emoji: '👍',
    }
    const view = renderRow({ preferences })

    pointer(view.body, 'down', 130)
    pointer(view.body, 'move', 45)
    pointer(view.body, 'up', 45)

    expect(view.onOpenActions).toHaveBeenCalledOnce()
    expect(view.getByRole('button', { name: 'Confirm delete' })).toBeTruthy()
    expect(view.timeline.redact).not.toHaveBeenCalled()
  })

  it('defers an image tap to open media but uses a double tap to edit its caption', () => {
    vi.useFakeTimers()
    const preferences: MessageGesturePreferences = {
      schema_version: 1,
      bindings: {
        double_tap: 'edit',
        touch_and_hold: 'thread',
        swipe_left: 'reply',
      },
      reaction_emoji: '👍',
    }
    const media = event({
      body: 'A caption',
      content: {
        msgtype: 'm.image',
        body: 'A caption',
        filename: 'photo.png',
        url: 'mxc://hs/photo',
      } as unknown as TimelineEvent['content'],
    })
    const view = renderRow({ preferences, rowEvent: media })
    const open = document.createElement('button')
    open.className = 'media-open'
    const onOpen = vi.fn()
    open.addEventListener('click', onOpen)
    view.body.append(open)

    expect(view.row.classList.contains('touch-hold-enabled')).toBe(true)

    pointer(open, 'down', 40)
    pointer(open, 'up', 40)
    fireEvent.click(open)
    expect(onOpen).not.toHaveBeenCalled()
    act(() => {
      vi.advanceTimersByTime(300)
    })
    expect(onOpen).toHaveBeenCalledOnce()
    expect(view.onOpenActions).not.toHaveBeenCalled()

    onOpen.mockClear()
    pointer(open, 'down', 40)
    pointer(open, 'up', 40)
    fireEvent.click(open)
    pointer(open, 'down', 41)
    pointer(open, 'up', 41)
    fireEvent.click(open)

    expect(view.onEdit).toHaveBeenCalledWith(
      expect.objectContaining({ event_id: '$event', body: 'A caption' }),
    )
    expect(onOpen).not.toHaveBeenCalled()
    act(() => {
      vi.advanceTimersByTime(300)
    })
    expect(onOpen).not.toHaveBeenCalled()
  })

  it('still excludes failed sends from custom gestures', () => {
    const failedView = renderRow({
      rowEvent: event({
        localEcho: { status: 'failed', body: 'hello', options: {} },
      }),
    })
    expect(failedView.row.classList.contains('touch-hold-enabled')).toBe(false)
  })
})
