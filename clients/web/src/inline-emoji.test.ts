import { describe, expect, it } from 'vitest'
import {
  parseInlineEmoji,
  rebaseInlineEmoji,
  inlineEmojiFromEvent,
  formatInlineEmoji,
  inlineEmojiDraftFits,
} from './inline-emoji'
import { formatMessageBody } from './mentions'

const context = {
  accountId: 'a',
  members: [],
  rooms: [],
  roomTitles: new Map<string, string>(),
}
const ref = { start: 3, end: 10, shortcode: 'smile', mxc: 'mxc://hs/custom' }

describe('inline custom emoji', () => {
  it('ignores quoted reply images and shortcodes when recovering edit bindings', () => {
    const body = 'hi :smile:'
    const sent = formatMessageBody(body, { ...context, inlineEmoji: [ref] })
    for (const quoted of [
      ':smile:',
      '<img data-mx-emoticon src="mxc://hs/quoted" alt=":smile:">',
    ]) {
      expect(
        inlineEmojiFromEvent(body, {
          ...sent,
          formatted_body: `<mx-reply><blockquote>${quoted}</blockquote></mx-reply>${sent.formatted_body}`,
        }),
      ).toEqual([ref])
    }
  })
  it('does not send user-typed MXC Markdown images or links, alongside bound emoji', () => {
    const body = 'hi :smile: ![x](mxc://hs/arbitrary) [x](mxc://hs/link)'
    const sent = formatMessageBody(body, { ...context, inlineEmoji: [ref] })
    const doc = new DOMParser().parseFromString(
      sent.formatted_body!,
      'text/html',
    )
    expect(
      [...doc.querySelectorAll('img[src]')].map((img) =>
        img.getAttribute('src'),
      ),
    ).toEqual([ref.mxc])
    expect(doc.querySelector('a[href^="mxc:"]')).toBeNull()
  })
  it('accounts for serialized draft bytes, including long MXCs and multibyte text', () => {
    expect(inlineEmojiDraftFits('hi :smile:', [ref])).toBe(true)
    expect(
      inlineEmojiDraftFits('x'.repeat(64_500) + ':smile:', [
        {
          ...ref,
          start: 64_500,
          end: 64_507,
          mxc: 'mxc://hs/' + 'x'.repeat(2000),
        },
      ]),
    ).toBe(false)
    expect(inlineEmojiDraftFits('🙂'.repeat(20_000), [])).toBe(false)
  })
  it('recovers exact image identities when editing sent text with repeated names and URL destinations', () => {
    const body = '[:smile:](https://hs/:smile:) :smile: :smile:'
    const first = body.lastIndexOf(':smile:', body.length - 8)
    const refs = [
      { ...ref, start: first, end: first + 7 },
      {
        ...ref,
        start: body.length - 7,
        end: body.length,
        mxc: 'mxc://other/custom',
      },
    ]
    const sent = formatMessageBody(body, { ...context, inlineEmoji: refs })
    expect(inlineEmojiFromEvent(sent.body, sent)).toEqual(
      refs.map((item, i) => ({
        ...item,
        start:
          i === 0
            ? sent.body.lastIndexOf(':smile:', sent.body.length - 8)
            : sent.body.length - 7,
        end:
          i === 0
            ? sent.body.lastIndexOf(':smile:', sent.body.length - 8) + 7
            : sent.body.length,
      })),
    )
  })
  it('preserves identity through formatting and exact edits in repeated text', () => {
    const body = ':smile: :smile:'
    const refs = [
      { ...ref, start: 0, end: 7 },
      { ...ref, start: 8, end: 15, mxc: 'mxc://other/custom' },
    ]
    expect(
      rebaseInlineEmoji(body, ':smile: ' + body, refs, { start: 0, end: 0 }),
    ).toEqual(
      refs.map((ref) => ({ ...ref, start: ref.start + 8, end: ref.end + 8 })),
    )
    expect(
      formatInlineEmoji(':smile:', [refs[0]], {
        from: 0,
        to: 7,
        insert: '**:smile:**',
      }),
    ).toEqual([{ ...refs[0], start: 2, end: 9 }])
  })
  it('preserves a selected image and its readable fallback alongside Markdown', () => {
    const formatted = formatMessageBody('hi :smile: **there**', {
      ...context,
      inlineEmoji: [ref],
    })
    expect(formatted.body).toBe('hi :smile: **there**')
    expect(formatted.formatted_body).toContain('<strong>there</strong>')
    const template = document.createElement('template')
    template.innerHTML = formatted.formatted_body!
    const img = template.content.querySelector('img')!
    expect(img.getAttribute('src')).toBe(ref.mxc)
    expect(img.hasAttribute('data-mx-emoticon')).toBe(true)
    expect(img.alt).toBe(':smile:')
    expect(img.height).toBe(24)
    expect(formatted.formatted_body).not.toContain('AxonEmoji')
    expect(formatted.formatted_body).not.toContain('data-mxc')
  })
  it('keeps two identically named selections bound to different packs', () => {
    const text = ':smile: :smile:'
    const formatted = formatMessageBody(text, {
      ...context,
      inlineEmoji: [
        { ...ref, start: 0, end: 7 },
        { ...ref, start: 8, end: 15, mxc: 'mxc://other/image' },
      ],
    })
    expect(formatted.body).toBe(text)
    expect(formatted.formatted_body).toContain('src="mxc://hs/custom"')
    expect(formatted.formatted_body).toContain('src="mxc://other/image"')
  })
  it.each(['`:smile:`', '[link](https://example.org/:smile:)', '\\:smile:'])(
    'keeps code, destinations, and escapes literal: %s',
    (text) => {
      const start = text.indexOf(':smile:')
      const formatted = formatMessageBody(text, {
        ...context,
        inlineEmoji: [{ ...ref, start, end: start + 7 }],
      })
      expect(formatted.formatted_body ?? '').not.toContain('<img')
      expect(formatted.formatted_body ?? '').not.toContain('AxonEmoji')
      expect(formatted.body).not.toContain('AxonEmoji')
    },
  )
  it('rejects hostile or stale draft metadata before rendering', () => {
    expect(
      parseInlineEmoji('hi :smile:', [
        { ...ref, mxc: 'https://tracker.test/image' },
        { ...ref, start: -1 },
        { ...ref, end: 100 },
      ]),
    ).toEqual([])
    expect(parseInlineEmoji('hi :other:', [ref])).toEqual([])
  })
  it('moves references with edits and loses their association if a token is changed', () => {
    const before = 'hi :smile:'
    expect(rebaseInlineEmoji(before, '🙂 ' + before, [ref])).toEqual([
      { ...ref, start: 6, end: 13 },
    ])
    expect(rebaseInlineEmoji(before, 'hi :smileX:', [ref])).toEqual([])
    expect(rebaseInlineEmoji(before, 'hi ', [ref])).toEqual([])
  })
})
