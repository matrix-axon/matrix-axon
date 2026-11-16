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
        { matched_url: A, 'beeper:image:encryption': { url: 'mxc://s/m' } },
        { matched_url: B, 'og:url': 'extra key' },
      ],
    })
    expect(field.hasField).toBe(true)
    expect([...field.entries.keys()]).toEqual([A])
    expect(field.entries.get(A)).toEqual({
      'matrix:matched_url': A,
      'matrix:image:encrypted': { url: 'mxc://s/m' },
    })
  })

  it('keeps only the first eight entries', () => {
    const urls = Array.from({ length: 10 }, (_, n) => `https://e.org/${n}`)
    const field = messagePreviewField({ 'm.url_previews': urls.map(entry) })
    expect([...field.entries.keys()]).toEqual(urls.slice(0, 8))
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
