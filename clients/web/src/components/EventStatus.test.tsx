import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
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
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
})

describe('EventTime event permalink copy', () => {
  it('copies a Matrix.to room-event link from confirmed timestamps', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    const { getByRole, findByRole } = render(<EventTime event={event()} />)

    fireEvent.click(
      getByRole('button', { name: 'Copy Matrix.to link to event' }),
    )

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

    fireEvent.click(
      getByRole('button', { name: 'Copy Matrix.to link to event' }),
    )

    expect((await findByRole('status')).textContent).toBe('Copy failed')
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

    expect(
      queryByRole('button', { name: 'Copy Matrix.to link to event' }),
    ).toBeNull()
    expect(getByText('7:26am')).toBeTruthy()
  })
})
