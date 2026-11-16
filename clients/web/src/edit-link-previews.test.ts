import { describe, expect, it } from 'vitest'
import {
  editFits,
  messageCards,
  messagePreviewField,
  planEditPreviews,
  wireEditPreviews,
  type EditPreviewDraft,
  type EditRequest,
} from './edit-link-previews'
import type { OutgoingLinkPreview } from './link-preview-send'

const A = 'https://a.example.org/x'
const B = 'https://b.example.org/y'
const C = 'https://c.example.org/z'

const entry = (url: string) => ({
  'matrix:matched_url': url,
  'og:title': `Card ${url}`,
})

const request = (
  body: string,
  content: unknown = { msgtype: 'm.text', body },
  formattedBody: string | null = null,
): EditRequest => ({ eventId: '$e', body, formattedBody, content })

const draft = (
  fields: Partial<EditPreviewDraft> & { opened?: Set<string> } = {},
): EditPreviewDraft => ({
  cards: [],
  kept: [],
  opened: new Set(),
  discarded: new Set(),
  ...fields,
})

const fresh = (
  url: string,
  image = false,
  description: string | null = null,
): OutgoingLinkPreview => ({
  matchedUrl: url,
  title: `New ${url}`,
  description,
  siteName: null,
  image: image ? { bytes: new Uint8Array(4), mimeType: 'image/png' } : null,
})

const plan = (
  req: EditRequest,
  d: EditPreviewDraft,
  originalText = req.body,
  previewsOn = true,
  allowUnmarked = false,
) => planEditPreviews(req, d, { originalText, previewsOn, allowUnmarked })

describe('messagePreviewField', () => {
  it('reads the standard field as authoritative, even when malformed', () => {
    expect(messagePreviewField({ body: 'x' })).toEqual({
      hasField: false,
      entries: new Map(),
    })
    expect(
      messagePreviewField({
        'm.url_previews': 'nope',
        'com.beeper.linkpreviews': [{ matched_url: A }],
      }),
    ).toEqual({ hasField: true, entries: new Map() })
  })

  it('converts the alias, and leaves out what the server would not keep', () => {
    const field = messagePreviewField({
      'com.beeper.linkpreviews': [
        {
          matched_url: A,
          'beeper:image:encryption': {
            url: 'mxc://s/m',
            key: {},
            iv: 'AAAA',
            hashes: { sha256: 'h' },
            v: 'v2',
          },
        },
        { matched_url: B, 'og:url': 'extra key' },
      ],
    })
    expect(field.hasField).toBe(true)
    expect([...field.entries.keys()]).toEqual([A])
    expect(field.entries.get(A)).toEqual({
      'matrix:matched_url': A,
      'matrix:image:encrypted': {
        url: 'mxc://s/m',
        key: {},
        iv: 'AAAA',
        hashes: { sha256: 'h' },
        v: 'v2',
      },
    })
  })

  it('keeps only the first eight entries', () => {
    const urls = Array.from({ length: 10 }, (_, n) => `https://e.org/${n}`)
    const field = messagePreviewField({ 'm.url_previews': urls.map(entry) })
    expect([...field.entries.keys()]).toEqual(urls.slice(0, 8))
  })
})

describe('messagePreviewField mirrors the server checks', () => {
  const encrypted = {
    url: 'mxc://example.org/enc',
    key: { kty: 'oct' },
    iv: 'AAAA',
    hashes: { sha256: 'hash' },
    v: 'v2',
  }
  const image = {
    'og:image': 'mxc://example.org/abc',
    'og:image:type': 'image/png',
    'og:image:width': 10,
    'og:image:height': 10,
    'matrix:image:size': 100,
  }

  it.each([
    ['a title past 300 units', { 'og:title': 't'.repeat(301) }],
    ['a description past 1000 units', { 'og:description': 'd'.repeat(1001) }],
    ['a site name past 120 units', { 'og:site_name': 's'.repeat(121) }],
    ['a title that is not text', { 'og:title': 7 }],
    ['an image that is not mxc', { 'og:image': 'https://a.example/x.png' }],
    ['an mxc with a path', { 'og:image': 'mxc://example.org/a/b' }],
    ['both image keys', { ...image, 'matrix:image:encrypted': encrypted }],
    ['an SVG type', { 'og:image:type': 'image/svg+xml' }],
    ['a zero width', { 'og:image:width': 0 }],
    ['too many pixels', { 'og:image:width': 4000, 'og:image:height': 4000 }],
    ['an image past 2 MiB', { 'matrix:image:size': 3 * 1024 * 1024 }],
    [
      'a descriptor with an extra key',
      { 'matrix:image:encrypted': { ...encrypted, extra: 1 } },
    ],
    [
      'a descriptor without its hash',
      { 'matrix:image:encrypted': { ...encrypted, hashes: {} } },
    ],
    [
      'credentials in the link',
      { 'matrix:matched_url': 'https://u:p@a.example/' },
    ],
    ['a non-web link', { 'matrix:matched_url': 'ftp://a.example/' }],
  ])('leaves out an entry with %s', (_, fields) => {
    const content = {
      'm.url_previews': [{ 'matrix:matched_url': A, ...fields }],
    }
    expect(messagePreviewField(content).entries.size).toBe(0)
    // The timeline still renders some of these; edit mode does not offer
    // a card the server would drop on save.
    expect(messageCards({ msgtype: 'm.text', body: A, ...content }, A)).toEqual(
      [],
    )
  })

  it.each([
    ['text only', { 'og:title': 'Title' }],
    ['a plain image', image],
    ['an encrypted image', { 'matrix:image:encrypted': encrypted }],
  ])('keeps an entry with %s', (_, fields) => {
    const content = {
      'm.url_previews': [{ 'matrix:matched_url': A, ...fields }],
    }
    expect([...messagePreviewField(content).entries.keys()]).toEqual([A])
  })
})

describe('messageCards', () => {
  it('shows the cards the timeline renders and the server keeps', () => {
    const content = {
      msgtype: 'm.text',
      body: `${A} ${B}`,
      'm.url_previews': [entry(A), { ...entry(B), 'og:url': 'x' }],
    }
    expect(messageCards(content, `${A} ${B}`).map((c) => c.url)).toEqual([A])
  })

  it('shows none for a message with a spoiler', () => {
    const content = {
      msgtype: 'm.text',
      body: A,
      format: 'org.matrix.custom.html',
      formatted_body: `<span data-mx-spoiler>${A}</span>`,
      'm.url_previews': [entry(A)],
    }
    expect(messageCards(content, A)).toEqual([])
  })
})

describe('planEditPreviews', () => {
  const withCards = (body: string, ...urls: string[]) => ({
    msgtype: 'm.text',
    body,
    'm.url_previews': urls.map(entry),
  })

  it("keeps the message's cards with an unrelated edit (rule 2)", () => {
    const req = request(`now ${A}`, withCards(`then ${A}`, A))
    expect(
      plan(req, draft({ kept: [A], opened: new Set([A]) }), `then ${A}`),
    ).toEqual({ kind: 'send', previews: [{ kind: 'keep', matchedUrl: A }] })
  })

  it('sends kept and new cards in text order (rule 2)', () => {
    const req = request(`${B} ${A}`, withCards(A, A))
    expect(
      plan(
        req,
        draft({ kept: [A], cards: [fresh(B)], opened: new Set([A]) }),
        A,
      ),
    ).toEqual({
      kind: 'send',
      previews: [
        { kind: 'new', preview: fresh(B) },
        { kind: 'keep', matchedUrl: A },
      ],
    })
  })

  it('keeps a card whose link new lookups would refuse', () => {
    // The fetch policy refuses plain http for new cards; the server keeps
    // the message's own card for it.
    const HTTP = 'http://plain.example.org/page'
    const req = request(`edited ${HTTP}`, withCards(HTTP, HTTP))
    expect(
      plan(req, draft({ kept: [HTTP], opened: new Set([HTTP]) }), HTTP),
    ).toEqual({ kind: 'send', previews: [{ kind: 'keep', matchedUrl: HTTP }] })
  })

  it('sends [] when a card is removed (rule 3)', () => {
    const req = request(A, withCards(A, A))
    expect(
      plan(req, draft({ opened: new Set([A]), discarded: new Set([A]) })),
    ).toEqual({ kind: 'send', previews: [] })
  })

  it('sends [] for an added link without a card, with previews on (rule 3)', () => {
    const req = request(`x ${B}`, { msgtype: 'm.text', body: 'x' })
    expect(plan(req, draft(), 'x')).toEqual({ kind: 'send', previews: [] })
    // Off, an added link changes nothing.
    expect(plan(req, draft(), 'x', false)).toEqual({ kind: 'omit' })
  })

  it('omits the field when the cards and links are untouched (rule 4)', () => {
    const req = request(`edited ${A}`, { msgtype: 'm.text', body: A })
    expect(plan(req, draft(), A)).toEqual({ kind: 'omit' })
    // An opted-out message keeps its marker through carry-forward.
    const optedOut = request(`edited ${A}`, {
      msgtype: 'm.text',
      body: A,
      'm.url_previews': [],
    })
    expect(plan(optedOut, draft(), A)).toEqual({ kind: 'omit' })
  })

  it('sends [] for a spoiler only when the message has a field (rule 1)', () => {
    const html = `<span data-mx-spoiler>${A}</span>`
    expect(
      plan(
        request(A, withCards(A, A), html),
        draft({ kept: [A], opened: new Set([A]) }),
      ),
    ).toEqual({ kind: 'send', previews: [] })
    expect(
      plan(request(A, { msgtype: 'm.text', body: 'x' }, html), draft(), 'x'),
    ).toEqual({ kind: 'omit' })
  })

  it('drops a kept card whose link left the body', () => {
    const req = request('gone', withCards(A, A))
    expect(plan(req, draft({ kept: [A], opened: new Set([A]) }), A)).toEqual({
      kind: 'send',
      previews: [],
    })
  })

  it("degrades new images, then new cards, then the message's cards", () => {
    // Long enough that only a little room is left for previews.
    const long = 'x'.repeat(19_000)
    const body = `${long} ${A} ${B} ${C}`
    const content = withCards(body, A)
    const d = draft({
      kept: [A],
      cards: [fresh(B, true, 'd'.repeat(900)), fresh(C, true)],
      opened: new Set([A]),
    })
    const result = plan(request(body, content), d, body)
    expect(result.kind).toBe('send')
    const previews = result.kind === 'send' ? result.previews : []
    // Images go first, from the last card back.
    expect(
      previews.map((p) =>
        p.kind === 'keep'
          ? 'keep'
          : `${p.preview.matchedUrl} ${p.preview.image !== null}`,
      ),
    ).toEqual(['keep', `${B} false`, `${C} false`])
    // With no room even for one card, every card goes, and [] is sent.
    const longer = `${'x'.repeat(20_300)} ${A}`
    expect(
      plan(
        request(longer, withCards(longer, A)),
        draft({ kept: [A], opened: new Set([A]) }),
        longer,
      ),
    ).toEqual({ kind: 'send', previews: [] })
  })

  it('asks before an edit too long for [] would lose its marker', () => {
    const huge = `${'x'.repeat(21_000)} ${A}`
    const req = request(huge, withCards(huge, A))
    const d = draft({ kept: [A], opened: new Set([A]) })
    expect(plan(req, d, huge)).toEqual({ kind: 'too-long-for-marker' })
    expect(plan(req, d, huge, true, true)).toEqual({ kind: 'omit' })
    // No field and no discarded link: nothing to lose, so the field is
    // simply left out.
    const plain = request(`${'x'.repeat(21_000)} ${B}`, {
      msgtype: 'm.text',
      body: 'x',
    })
    expect(plan(plain, draft(), 'x')).toEqual({ kind: 'omit' })
  })
})

describe('editFits', () => {
  it('counts the body twice, as the replacement carries it', () => {
    const body = 'x'.repeat(20_000)
    expect(editFits(request(body), [])).toBe(true)
    expect(editFits(request('x'.repeat(20_600)), [])).toBe(false)
  })
})

describe('wireEditPreviews', () => {
  it('maps keep entries and new cards with their upload ids', () => {
    expect(
      wireEditPreviews(
        [
          { kind: 'keep', matchedUrl: A },
          { kind: 'new', preview: fresh(B, true) },
          { kind: 'new', preview: fresh(C) },
        ],
        ['upload-1', null],
      ),
    ).toEqual([
      { matched_url: A, keep: true },
      { matched_url: B, title: `New ${B}`, image_upload_id: 'upload-1' },
      { matched_url: C, title: `New ${C}` },
    ])
  })
})
