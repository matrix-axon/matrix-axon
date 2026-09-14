import {
  act,
  cleanup,
  fireEvent,
  render,
  waitFor,
} from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { TimelineEvent } from '../stores/timeline'
import { EventTime, formatEventTime } from './EventStatus'

describe('formatEventTime', () => {
  it('renders compact 12-hour row timestamps', () => {
    expect(formatEventTime(new Date(2026, 6, 16, 7, 26).getTime())).toBe(
      '7:26am',
    )
    expect(formatEventTime(new Date(2026, 6, 16, 17, 5).getTime())).toBe(
      '5:05pm',
    )
    expect(formatEventTime(new Date(2026, 6, 16, 0, 0).getTime())).toBe(
      '12:00am',
    )
    expect(formatEventTime(new Date(2026, 6, 16, 12, 0).getTime())).toBe(
      '12:00pm',
    )
  })
})

function event(overrides: Partial<TimelineEvent> = {}): TimelineEvent {
  return {
    account_id: 'acct',
    event_id: '$event',
    room_id: '!room:hs',
    sender: '@alice:hs',
    origin_ts: new Date(2026, 6, 16, 7, 26).getTime(),
    arrival_order: new Date(2026, 6, 16, 7, 26).getTime(),
    type: 'm.room.message',
    body: 'hello',
    content: { msgtype: 'm.text', body: 'hello' },
    redacted: false,
    edited: false,
    edit_count: 0,
    ...overrides,
  } as TimelineEvent
}

afterEach(() => {
  cleanup()
  vi.useRealTimers()
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
})

describe('EventTime event permalink copy', () => {
  it('copies a Matrix.to room-event link from confirmed timestamps', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    const { getByRole, findByRole } = render(<EventTime event={event()} />)

    fireEvent.click(getByRole('button', { name: 'Copy link' }))

    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith(
        'https://matrix.to/#/!room%3Ahs/%24event?via=hs',
      ),
    )
    expect((await findByRole('status')).textContent).toBe('Copied')
  })

  it('shows copy failure feedback', async () => {
    vi.stubGlobal('navigator', {
      clipboard: { writeText: vi.fn().mockRejectedValue(new Error('denied')) },
    })
    const { getByRole, findByRole } = render(<EventTime event={event()} />)

    fireEvent.click(getByRole('button', { name: 'Copy link' }))

    expect((await findByRole('status')).textContent).toBe('Copy failed')
  })

  it('copies the message body on an enabled timestamp hold without copying the link', async () => {
    vi.useFakeTimers()
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    const { getByRole, getByText } = render(
      <EventTime event={event()} touchHoldEnabled />,
    )
    const timestamp = getByRole('button', {
      name: 'Copy link',
    })

    fireEvent.pointerDown(timestamp, {
      pointerId: 1,
      pointerType: 'touch',
      isPrimary: true,
      clientX: 20,
      clientY: 20,
    })
    await act(async () => {
      vi.advanceTimersByTime(550)
      await Promise.resolve()
    })
    fireEvent.pointerUp(timestamp, {
      pointerId: 1,
      pointerType: 'touch',
      isPrimary: true,
      clientX: 20,
      clientY: 20,
    })
    fireEvent.click(timestamp)

    expect(writeText).toHaveBeenCalledOnce()
    expect(writeText).toHaveBeenCalledWith('hello')
    expect(getByText('Text copied')).toBeTruthy()
  })

  it('copies the message body on a desktop double-click without copying the link', async () => {
    vi.useFakeTimers()
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    const { getByRole, getByText } = render(
      <EventTime event={event()} touchHoldEnabled />,
    )
    const timestamp = getByRole('button', {
      name: 'Copy link',
    })

    fireEvent.pointerDown(timestamp, {
      pointerId: 1,
      pointerType: 'mouse',
      isPrimary: true,
    })
    fireEvent.click(timestamp, { detail: 1 })
    fireEvent.pointerDown(timestamp, {
      pointerId: 1,
      pointerType: 'mouse',
      isPrimary: true,
    })
    fireEvent.click(timestamp, { detail: 2 })
    fireEvent(
      timestamp,
      new MouseEvent('dblclick', {
        bubbles: true,
        cancelable: true,
        detail: 2,
      }),
    )
    await act(async () => await Promise.resolve())

    expect(writeText).toHaveBeenCalledOnce()
    expect(writeText).toHaveBeenCalledWith('hello')
    expect(getByText('Text copied')).toBeTruthy()
    act(() => {
      vi.advanceTimersByTime(300)
    })
    expect(writeText).toHaveBeenCalledOnce()
  })

  it('copies the event link after a desktop single-click settles', async () => {
    vi.useFakeTimers()
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    const { getByRole } = render(<EventTime event={event()} />)
    const timestamp = getByRole('button', {
      name: 'Copy link',
    })

    fireEvent.pointerDown(timestamp, {
      pointerId: 1,
      pointerType: 'mouse',
      isPrimary: true,
    })
    fireEvent.click(timestamp, { detail: 1 })
    expect(writeText).not.toHaveBeenCalled()

    await act(async () => {
      vi.advanceTimersByTime(300)
      await Promise.resolve()
    })
    expect(writeText).toHaveBeenCalledWith(
      'https://matrix.to/#/!room%3Ahs/%24event?via=hs',
    )
  })

  it('keeps the timestamp native context menu on desktop when hold is enabled', () => {
    const { getByRole } = render(<EventTime event={event()} touchHoldEnabled />)
    const timestamp = getByRole('button', {
      name: 'Copy link',
    })
    const contextMenu = new MouseEvent('contextmenu', {
      bubbles: true,
      cancelable: true,
    })

    timestamp.dispatchEvent(contextMenu)

    expect(contextMenu.defaultPrevented).toBe(false)
  })

  it('suppresses the timestamp context menu during an enabled touch hold', () => {
    const { getByRole } = render(<EventTime event={event()} touchHoldEnabled />)
    const timestamp = getByRole('button', {
      name: 'Copy link',
    })
    fireEvent.pointerDown(timestamp, {
      pointerId: 1,
      pointerType: 'touch',
      isPrimary: true,
    })
    const contextMenu = new MouseEvent('contextmenu', {
      bubbles: true,
      cancelable: true,
    })

    timestamp.dispatchEvent(contextMenu)

    expect(contextMenu.defaultPrevented).toBe(true)
  })

  it('does not offer a copy control for local echoes', () => {
    const { queryByRole, getByText } = render(
      <EventTime
        event={event({
          event_id: 'local:1',
          localEcho: {
            status: 'failed',
            body: 'hello',
            options: {},
          },
        })}
      />,
    )

    expect(queryByRole('button', { name: 'Copy link' })).toBeNull()
    expect(getByText('7:26am')).toBeTruthy()
  })
})
