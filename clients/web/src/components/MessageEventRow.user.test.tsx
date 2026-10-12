import { signal } from '@preact/signals'
import { cleanup, fireEvent, render } from '@testing-library/preact'
import { afterEach, expect, it, vi } from 'vitest'
import { ServicesContext } from '../services'
import type { MembersStore } from '../stores/members'
import type { TimelineEvent, TimelineStore } from '../stores/timeline'
import { testServices } from '../test/services'
import { MessageEventRow } from './MessageEventRow'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const ALICE = '@alice:hs'
const ME = '@me:hs'

const members = {
  displayName: (userId: string) => (userId === ALICE ? 'Alice' : userId),
  members: signal(new Map()),
} as unknown as MembersStore

function event(overrides: Partial<TimelineEvent> = {}): TimelineEvent {
  return {
    account_id: ACCOUNT,
    event_id: '$event',
    room_id: '!room:hs',
    sender: ALICE,
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

function renderRow(
  rowEvent: TimelineEvent,
  onOpenUser?: (userId: string) => void,
) {
  const services = testServices()
  const parent = event({
    event_id: '$parent',
    body: 'earlier',
    sender: ALICE,
  })
  const timeline = {
    toggleReaction: vi.fn().mockResolvedValue(true),
    redact: vi.fn(),
    replyTargets: signal(new Map([['$parent', parent]])),
  } as unknown as TimelineStore
  const onOpenActions = vi.fn()
  const view = render(
    <ServicesContext.Provider value={services}>
      <ol>
        <MessageEventRow
          event={rowEvent}
          timeline={timeline}
          members={members}
          accountId={ACCOUNT}
          ownUserId={ME}
          settings={services.settings}
          messageGestures={null}
          reactionPickerOpen={false}
          onSetReactionPicker={() => {}}
          actionsOpen={false}
          onOpenActions={onOpenActions}
          onReply={() => {}}
          onEdit={() => {}}
          onOpenUser={onOpenUser}
        />
      </ol>
    </ServicesContext.Provider>,
  )
  return { ...view, onOpenActions, timeline }
}

afterEach(() => {
  cleanup()
})

it('opens user information from the sender name and avatar without the action bar', () => {
  const onOpenUser = vi.fn()
  const view = renderRow(event(), onOpenUser)
  const name = view.getByRole('button', { name: 'Alice' })
  const avatar = view.container.querySelector('.event-person')
  expect(avatar).not.toBeNull()
  expect(avatar!.getAttribute('tabindex')).toBe('-1')
  expect(avatar!.getAttribute('aria-hidden')).toBe('true')
  expect(view.getAllByRole('button', { name: 'Alice' })).toHaveLength(1)
  fireEvent.click(name)
  fireEvent.click(avatar!)
  expect(onOpenUser).toHaveBeenCalledTimes(2)
  expect(onOpenUser).toHaveBeenNthCalledWith(1, ALICE, name, true)
  expect(onOpenUser).toHaveBeenNthCalledWith(2, ALICE, avatar, true)
  expect(view.onOpenActions).not.toHaveBeenCalled()
})

it('opens user information from a reply quote and leaves reaction taps alone', () => {
  const onOpenUser = vi.fn()
  const view = renderRow(
    event({
      relates_to: {
        'm.in_reply_to': { event_id: '$parent' },
      } as unknown as TimelineEvent['relates_to'],
      reactions: {
        '👍': {
          count: 1,
          me: false,
          senders: [ALICE],
          my_event_ids: [],
        },
      },
    }),
    onOpenUser,
  )
  const quote = view.container.querySelector('.reply-context')
  expect(quote).not.toBeNull()
  const quoteButtons = [...quote!.querySelectorAll('button')]
  expect(quoteButtons).toHaveLength(2)
  for (const button of quoteButtons) {
    fireEvent.click(button)
  }
  expect(onOpenUser).toHaveBeenCalledTimes(2)
  expect(onOpenUser).toHaveBeenNthCalledWith(1, ALICE, quoteButtons[0], true)
  expect(onOpenUser).toHaveBeenNthCalledWith(2, ALICE, quoteButtons[1], true)
  fireEvent.click(view.getByRole('button', { name: /👍/ }))
  expect(onOpenUser).toHaveBeenCalledTimes(2)
  expect(view.timeline.toggleReaction).toHaveBeenCalledOnce()
  expect(view.onOpenActions).not.toHaveBeenCalled()
})

it('leaves the sender as a label when nothing opens user information', () => {
  const view = renderRow(event())
  expect(
    view.queryByRole('button', { name: 'User information for Alice' }),
  ).toBeNull()
  expect(view.container.querySelector('.event-sender')?.textContent).toBe(
    'Alice',
  )
})
