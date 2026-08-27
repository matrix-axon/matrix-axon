import { cleanup, render } from '@testing-library/preact'
import { afterEach, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import type { EventDto } from '../stores/timeline'
import { testServices } from '../test/services'
import { EventBody, UTD_RECOVER_HINT } from './EventBody'

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

describe('EventBody UTD placeholder', () => {
  it('keeps the diagnosis and offers Recover keys as a link to Accounts', () => {
    const utd = {
      ...event(''),
      type: 'm.room.encrypted',
      body: null,
      content: null,
    }
    const { getByText, getByRole } = render(
      <ServicesContext.Provider value={testServices()}>
        <EventBody event={utd} />
      </ServicesContext.Provider>,
    )

    expect(getByText('unable to decrypt')).toBeTruthy()
    const recover = getByRole('link', {
      name: 'Recover keys',
    }) as HTMLAnchorElement
    expect(recover.getAttribute('href')).toBe('/accounts')
    expect(recover.title).toBe(UTD_RECOVER_HINT)
  })

  it('does not offer Recover keys on a redaction', () => {
    const gone = {
      ...event(''),
      redacted: true,
      body: null,
      content: null,
    }
    const { getByText, queryByRole } = render(
      <ServicesContext.Provider value={testServices()}>
        <EventBody event={gone} />
      </ServicesContext.Provider>,
    )

    expect(getByText('message deleted')).toBeTruthy()
    expect(queryByRole('link', { name: 'Recover keys' })).toBeNull()
  })
})

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
