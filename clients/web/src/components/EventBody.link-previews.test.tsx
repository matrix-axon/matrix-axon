import { render } from '@testing-library/preact'
import { describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import { testServices } from '../test/services'
import type { TimelineEvent } from '../stores/timeline'
import { EventBody } from './EventBody'

const url =
  'http://www.nbc.com/saturday-night-live/video/centaur-job-interview/n11485'
const title = 'Watch Centaur Job Interview from Saturday Night Live on NBC.com'
const preview = {
  matched_url: url,
  'og:title': title,
  'og:site_name': 'NBC',
  'og:description': 'A doctor interviews an applicant who is a centaur.',
}

function event(content: Record<string, unknown> = {}) {
  return {
    account_id: '11111111-1111-4111-8111-111111111111',
    room_id: '!room:hs',
    event_id: '$preview',
    sender: '@adam:hs',
    origin_ts: 1,
    arrival_order: 1,
    type: 'm.room.message',
    state_key: null,
    body: `[slack2matrix test] ${url}`,
    redacted: false,
    content: {
      msgtype: 'm.text',
      'com.beeper.linkpreviews': [preview],
      ...content,
    },
  } as unknown as TimelineEvent
}

function view(value = event()) {
  const services = testServices()
  const element = (e: TimelineEvent) => (
    <ServicesContext.Provider value={services}>
      <EventBody event={e} />
    </ServicesContext.Provider>
  )
  const rendered = render(element(value))
  return {
    ...rendered,
    update: (e: TimelineEvent) => rendered.rerender(element(e)),
  }
}

describe('bundled previews in the shared event renderer', () => {
  it('renders the Beeper example beneath its body with the actual destination', () => {
    const { container } = view()
    const card =
      container.querySelector<HTMLAnchorElement>('.link-preview-card')!
    expect(card).not.toBeNull()
    expect(card.href).toBe(url)
    expect(card.textContent).toContain(title)
    expect(card.textContent).toContain('NBC')
    expect(card.textContent).toContain('www.nbc.com')
    expect(card.textContent).toContain(preview['og:description'])
    expect(container.querySelector('.body-text')!.textContent).toContain(
      '[slack2matrix test]',
    )
    expect(card.rel).toBe('noopener noreferrer')
    expect(card.target).toBe('_blank')
  })

  it('replaces cards on edits and removes them when metadata disappears or a message is redacted', () => {
    const rendered = view()
    const edited = event({
      'com.beeper.linkpreviews': [
        { ...preview, 'og:title': 'Updated preview' },
      ],
    })
    rendered.update(edited)
    expect(
      rendered.container.querySelector('.link-preview-card')!.textContent,
    ).toContain('Updated preview')
    expect(rendered.queryByText(title)).toBeNull()
    rendered.update(event({ 'com.beeper.linkpreviews': [] }))
    expect(rendered.container.querySelector('.link-previews')).toBeNull()
    rendered.update(event())
    rendered.update({ ...event(), redacted: true })
    expect(rendered.container.querySelector('.link-previews')).toBeNull()
    expect(rendered.getByText('message deleted')).toBeTruthy()
  })

  it.each(['m.text', 'm.notice', 'm.emote', 'm.file'])(
    'supports %s including attachment captions',
    (msgtype) => {
      const { container } = view(
        event({ msgtype, url: 'mxc://hs/document', filename: 'document.bin' }),
      )
      expect(
        container.querySelector('.link-preview-card')!.textContent,
      ).toContain(title)
    },
  )

  it('renders metadata as text and ignores image and canonical URL fields', () => {
    const { container } = view(
      event({
        'com.beeper.linkpreviews': [
          {
            ...preview,
            'og:title': '<img src=x onerror=alert(1)>',
            'og:url': 'https://evil.test',
            'og:image': 'https://tracking.test/pixel',
          },
        ],
      }),
    )
    const card =
      container.querySelector<HTMLAnchorElement>('.link-preview-card')!
    expect(card.textContent).toContain('<img src=x onerror=alert(1)>')
    expect(card.href).toBe(url)
    expect(card.querySelector('img')).toBeNull()
  })

  it('withholds cards for formatted spoilers, state events, and undecrypted events', () => {
    const rendered = view(
      event({
        format: 'org.matrix.custom.html',
        formatted_body: `<span data-mx-spoiler>${url}</span>`,
      }),
    )
    expect(rendered.container.querySelector('.link-previews')).toBeNull()
    rendered.update({ ...event(), state_key: '' })
    expect(rendered.container.querySelector('.link-previews')).toBeNull()
    rendered.update({ ...event(), content: null })
    expect(rendered.container.querySelector('.link-previews')).toBeNull()
  })
})
