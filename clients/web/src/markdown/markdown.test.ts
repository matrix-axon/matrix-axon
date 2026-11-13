import { describe, expect, it } from 'vitest'
import {
  markdownCodeRanges,
  markdownToHtmlIfFormatted,
  markdownToPlainText,
} from './markdown'

describe('markdownToHtmlIfFormatted', () => {
  it('returns null for plain prose (TUI parity)', () => {
    expect(markdownToHtmlIfFormatted('hello world')).toBeNull()
    expect(markdownToHtmlIfFormatted('line one\nline two')).toBeNull()
    // Multiple paragraphs are still plain prose.
    expect(markdownToHtmlIfFormatted('para one\n\npara two')).toBeNull()
    // Escaped markdown reads as plain text.
    expect(markdownToHtmlIfFormatted('not \\*bold\\*')).toBeNull()
  })

  it('converts emphasis, code, links, and headings', () => {
    expect(markdownToHtmlIfFormatted('**bold** and _italic_')).toBe(
      '<p><strong>bold</strong> and <em>italic</em></p>',
    )
    expect(markdownToHtmlIfFormatted('`code`')).toBe('<p><code>code</code></p>')
    expect(markdownToHtmlIfFormatted('[x](https://example.org)')).toBe(
      '<p><a href="https://example.org">x</a></p>',
    )
    expect(markdownToHtmlIfFormatted('# heading')).toBe('<h1>heading</h1>')
  })

  it('converts lists, quotes, and fenced code blocks', () => {
    expect(markdownToHtmlIfFormatted('- a\n- b')).toContain('<ul>')
    expect(markdownToHtmlIfFormatted('> quoted')).toContain('<blockquote>')
    const fenced = markdownToHtmlIfFormatted('```rust\nfn main() {}\n```')
    expect(fenced).toContain('<pre><code class="language-rust">')
  })

  it('converts GFM strikethrough and tables', () => {
    expect(markdownToHtmlIfFormatted('~~gone~~')).toContain('<del>gone</del>')
    expect(
      markdownToHtmlIfFormatted('| a | b |\n| - | - |\n| 1 | 2 |'),
    ).toContain('<table>')
  })

  it('escapes raw inline HTML instead of passing it through', () => {
    const out = markdownToHtmlIfFormatted('**x** <script>alert(1)</script>')
    expect(out).not.toContain('<script>')
    expect(out).toContain('&lt;script&gt;')
  })

  it('never emits an active URI scheme in an outgoing href (WCR-15)', () => {
    const out = markdownToHtmlIfFormatted('[click](javascript:alert(1))')
    expect(out).not.toContain('javascript:')
    expect(out).toContain('click')

    // Ordinary links survive the outgoing sanitize untouched.
    expect(markdownToHtmlIfFormatted('[ok](https://example.org)')).toContain(
      'href="https://example.org"',
    )
  })
})

describe('markdownToPlainText', () => {
  it('strips Markdown syntax for one-line display', () => {
    expect(
      markdownToPlainText(
        '**bold** and _italic_ with [a link](https://e.test)',
      ),
    ).toBe('bold and italic with a link')
    expect(markdownToPlainText('- one\n- two')).toBe('one two')
    expect(markdownToPlainText('`code` and ~~gone~~')).toBe('code and gone')
  })

  it('keeps raw HTML-looking prose as text', () => {
    expect(markdownToPlainText('**x** <script>alert(1)</script>')).toBe(
      'x <script>alert(1)</script>',
    )
  })
})

describe('markdownCodeRanges', () => {
  it('finds code spans and fenced and indented blocks, in order', () => {
    const text = 'a `b` c\n\n```\nd\n```\n\n    e\n\n`f`'
    const ranges = markdownCodeRanges(text)
    expect(ranges.map(({ start, end }) => text.slice(start, end))).toEqual([
      '`b`',
      '```\nd\n```',
      '    e',
      '`f`',
    ])
  })
  it('finds none in prose', () => {
    expect(markdownCodeRanges('plain https://a.org text')).toEqual([])
  })
  it.each([
    ['a quote', '> ```\n> Https://x.org\n> ```', '```\n> Https://x.org\n> ```'],
    [
      'a list item',
      '- item\n\n  ```\n  Https://x.org\n  ```',
      '```\n  Https://x.org\n  ```',
    ],
    [
      'a quote in a list item',
      '1. a\n   > ```\n   > Https://x.org\n   > ```',
      '```\n   > Https://x.org\n   > ```',
    ],
    ['a quote, as a span', '> `Https://x.org`', '`Https://x.org`'],
  ])('finds a block inside %s', (_, text, code) => {
    const ranges = markdownCodeRanges(text)
    expect(ranges.map(({ start, end }) => text.slice(start, end))).toEqual([
      code,
    ])
  })
})
