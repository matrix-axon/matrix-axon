import { describe, expect, it } from 'vitest'
import {
  isPreviewRejection,
  planLinkPreviews,
  wirePreview,
  type OutgoingLinkPreview,
  type PreviewDraft,
} from './link-preview-send'
import { previewPreflight } from './incoming-share/contract'

const A = 'https://a.example.org/x'
const B = 'https://b.example.org/y'
const C = 'https://c.example.org/z'

function card(
  matchedUrl: string,
  extra: Partial<OutgoingLinkPreview> = {},
): OutgoingLinkPreview {
  return {
    matchedUrl,
    title: `Title of ${matchedUrl}`,
    description: null,
    siteName: null,
    image: null,
    ...extra,
  }
}

const image = {
  bytes: new Uint8Array([1, 2, 3]),
  mimeType: 'image/jpeg' as const,
}

/**
 * The shortest body from `make(n)` that `tooBig` rejects, found by bisection
 * (sizes only grow with `n`), which `fits` must still accept: the size at
 * which one more preview, image or relation tips the request over.
 */
function findBody(
  make: (n: number) => string,
  tooBig: (body: string) => boolean,
  fits: (body: string) => boolean,
): string {
  let low = 0
  let high = 41 * 1024
  if (!tooBig(make(high))) throw new Error('no body is too big')
  while (low < high) {
    const middle = Math.floor((low + high) / 2)
    if (tooBig(make(middle))) high = middle
    else low = middle + 1
  }
  const body = make(low)
  if (!fits(body)) throw new Error('no body of that size exists')
  return body
}

function draft(
  cards: OutgoingLinkPreview[],
  discarded: string[] = [],
): PreviewDraft {
  return { cards, discarded: new Set(discarded) }
}

describe('planLinkPreviews (ADR 0132 § 6)', () => {
  it('omits the field when the composer authors no previews', () => {
    expect(planLinkPreviews({ body: `see ${A}` }, undefined)).toEqual({
      kind: 'omit',
    })
  })

  it('omits the field for a message with a spoiler', () => {
    expect(
      planLinkPreviews(
        {
          body: `see ${A}`,
          format: 'org.matrix.custom.html',
          formatted_body: `<span data-mx-spoiler>see ${A}</span>`,
        },
        draft([card(A)]),
      ),
    ).toEqual({ kind: 'omit' })
  })

  it('sends kept cards in the order given', () => {
    const plan = planLinkPreviews(
      { body: `${A} and ${B}` },
      draft([card(A), card(B)]),
    )
    expect(plan).toEqual({ kind: 'send', previews: [card(A), card(B)] })
  })

  it('sends [] for an eligible link with no kept card', () => {
    expect(planLinkPreviews({ body: `see ${A} ` }, draft([]))).toEqual({
      kind: 'send',
      previews: [],
    })
  })

  it('omits the field when the body has no eligible link', () => {
    expect(
      planLinkPreviews({ body: 'see http://plain.example.org/' }, draft([])),
    ).toEqual({ kind: 'omit' })
  })

  it('counts a card only while its exact text is a bare link in the body', () => {
    // The formatted body is authoritative: a link the formatter changed, or
    // one the user removed after the card loaded, does not count.
    expect(
      planLinkPreviews({ body: `see ${A}/more` }, draft([card(A)])),
    ).toEqual({ kind: 'send', previews: [] })
    expect(planLinkPreviews({ body: 'no links' }, draft([card(A)]))).toEqual({
      kind: 'omit',
    })
  })

  it('never sends two cards for one destination', () => {
    const plan = planLinkPreviews(
      { body: `${A} https://A.EXAMPLE.ORG/x` },
      draft([card(A), card('https://A.EXAMPLE.ORG/x')]),
    )
    expect(plan).toEqual({ kind: 'send', previews: [card(A)] })
  })

  it('drops images from the last card first, then cards from the end', () => {
    // Size the body so three image reservations do not fit but two do.
    const big = (url: string) =>
      card(url, { description: 'd'.repeat(1000), image })
    const cards = [big(A), big(B), big(C)]
    const id = '00000000-0000-4000-8000-000000000001'
    const body = findBody(
      (n) => `${A} ${B} ${C} ${'x'.repeat(n)}`,
      (body) =>
        previewPreflight({
          body,
          url_previews: cards.map((c) => wirePreview(c, id)),
        }) !== null,
      (body) =>
        previewPreflight({
          body,
          url_previews: cards.map((c, i) => wirePreview(c, i < 2 ? id : null)),
        }) === null,
    )
    const plan = planLinkPreviews({ body }, draft(cards))
    expect(plan).toEqual({
      kind: 'send',
      previews: [big(A), big(B), { ...big(C), image: null }],
    })
  })

  it('drops cards once every image is gone', () => {
    const long = (url: string) => card(url, { description: 'd'.repeat(1000) })
    const cards = [long(A), long(B), long(C)]
    const body = findBody(
      (n) => `${A} ${B} ${C} ${'x'.repeat(n)}`,
      (body) =>
        previewPreflight({
          body,
          url_previews: cards.map((c) => wirePreview(c, null)),
        }) !== null,
      (body) =>
        previewPreflight({
          body,
          url_previews: cards.slice(0, 2).map((c) => wirePreview(c, null)),
        }) === null,
    )
    expect(planLinkPreviews({ body }, draft(cards))).toEqual({
      kind: 'send',
      previews: [long(A), long(B)],
    })
  })

  describe('a body too long even for []', () => {
    const body = `${A} ${'x'.repeat(41 * 1024)}`

    it('omits the field when the user discarded none of its links', () => {
      expect(planLinkPreviews({ body }, draft([card(A)]))).toEqual({
        kind: 'omit',
      })
    })

    it('asks the user when a discarded link is in the body', () => {
      expect(
        planLinkPreviews({ body }, draft([], ['https://a.example.org/x'])),
      ).toEqual({ kind: 'too-long-for-marker' })
    })

    it('matches the discard by identity, not by spelling', () => {
      expect(
        planLinkPreviews(
          { body: body.replace(A, 'https://A.example.org:443/x') },
          draft([], [A]),
        ),
      ).toEqual({ kind: 'too-long-for-marker' })
    })

    it('ignores a discarded link that is no longer in the body', () => {
      expect(planLinkPreviews({ body }, draft([], [B]))).toEqual({
        kind: 'omit',
      })
    })

    it('omits the field once the user agrees', () => {
      expect(planLinkPreviews({ body }, draft([], [A]), true)).toEqual({
        kind: 'omit',
      })
    })
  })

  it('counts reply and thread relations in the size check', () => {
    // Find a body that fits [] alone but not with both relations.
    const body = findBody(
      (n) => `${A} ${'x'.repeat(n)}`,
      (body) =>
        previewPreflight({
          body,
          url_previews: [],
          reply_to: '$event',
          thread_root: '$root',
        }) !== null,
      (body) => previewPreflight({ body, url_previews: [] }) === null,
    )
    expect(planLinkPreviews({ body }, draft([], [A]))).toEqual({
      kind: 'send',
      previews: [],
    })
    expect(
      planLinkPreviews(
        { body, reply_to: '$event', thread_root: '$root' },
        draft([], [A]),
      ),
    ).toEqual({ kind: 'too-long-for-marker' })
  })
})

describe('wirePreview', () => {
  it('leaves absent fields out', () => {
    expect(wirePreview(card(A, { title: null }), null)).toEqual({
      matched_url: A,
    })
    expect(
      wirePreview(card(A, { description: 'd', siteName: 's', image }), 'id'),
    ).toEqual({
      matched_url: A,
      title: `Title of ${A}`,
      description: 'd',
      site_name: 's',
      image_upload_id: 'id',
    })
  })
})

describe('isPreviewRejection', () => {
  it.each([
    [413, 'preview_too_large', true],
    [413, 'event_too_large', true],
    [413, 'payload_too_large', true],
    [400, 'bad_request', true],
    [404, 'not_found', true],
    [413, 'something_else', false],
    [403, 'forbidden', false],
    [500, 'internal', false],
    [undefined, undefined, false],
  ] as const)('%s %s → %s', (status, code, expected) => {
    expect(isPreviewRejection(status, code)).toBe(expected)
  })
})
