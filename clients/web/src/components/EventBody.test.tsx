import { cleanup, render } from '@testing-library/preact'
import { afterEach, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import type { EventDto } from '../stores/timeline'
import { testServices } from '../test/services'
import { EventBody } from './EventBody'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'

afterEach(cleanup)

function event(body: string): EventDto {
  return {
    account_id: ACCOUNT,
    event_id: '$event',
    room_id: '!room:hs',
    sender: '@alice:hs',
    origin_ts: 0,
    arrival_order: 0,
    type: 'm.room.message',
    body,
    content: { msgtype: 'm.text', body },
    redacted: false,
    edited: false,
    edit_count: 0,
  } as unknown as EventDto
}

function renderEventBody(body: string) {
  return render(
    <ServicesContext.Provider value={testServices()}>
      <EventBody event={event(body)} />
    </ServicesContext.Provider>,
  )
}

describe('EventBody emoji-only display', () => {
  it('marks an emoji-only message body for larger rendering', () => {
    const { container } = renderEventBody('👍')

    expect(container.querySelector('.body-emoji-only')?.textContent).toBe('👍')
  })

  it('leaves inline emoji inside text at normal message size', () => {
    const { container } = renderEventBody('ship it 👍')

    expect(container.querySelector('.body-emoji-only')).toBeNull()
  })
})
