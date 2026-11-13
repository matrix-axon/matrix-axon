import { Marked, type Token, type Tokens } from 'marked'
import { sanitizeOutgoingHtml } from '../html/sanitize'

/**
 * Markdown-on-send (ADR 0046, M-W7), mirroring the TUI's
 * `markdown_to_html_if_detected` (`clients/tui/src/html.rs`): the composer is
 * a plain textarea; on send, text that *contains Markdown formatting* is
 * converted to HTML for `formatted_body`, while plain prose — paragraphs,
 * text, and line breaks only — is sent as a bare `body` with no formatted
 * part. The server carries both verbatim (it never interprets Markdown).
 *
 * Raw inline HTML in the input is escaped, not passed through: the composer
 * is a Markdown surface, not an HTML one (receiving clients sanitize anyway;
 * this just keeps our own output honest).
 */

const escapeHtml = (text: string): string =>
  text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')

const md = new Marked({ gfm: true })
md.use({
  renderer: {
    html: (token: Tokens.HTML | Tokens.Tag) => escapeHtml(token.text),
  },
})

const BLOCK_END =
  /<\/(?:blockquote|br|dd|div|dl|dt|h[1-6]|li|ol|p|pre|table|td|th|tr|ul)>/gi

/** Block-level tokens that plain prose produces. */
const PLAIN_BLOCKS = new Set(['paragraph', 'space'])
/** Inline tokens that plain prose produces (`br` = trailing-space break). */
const PLAIN_INLINE = new Set(['text', 'br', 'escape'])

function isPlain(tokens: Token[]): boolean {
  for (const token of tokens) {
    if (token.type === 'paragraph') {
      const inline = (token as Tokens.Paragraph).tokens ?? []
      if (inline.some((t) => !PLAIN_INLINE.has(t.type))) {
        return false
      }
    } else if (!PLAIN_BLOCKS.has(token.type)) {
      return false
    }
  }
  return true
}

/**
 * Convert `text` to HTML if it contains Markdown formatting; `null` when it
 * is plain prose (only paragraphs, text, and line breaks — the TUI's rule),
 * in which case the message is sent without a `formatted_body`.
 */
export function markdownToHtmlIfFormatted(text: string): string | null {
  const tokens = md.lexer(text)
  if (isPlain(tokens)) {
    return null
  }
  // Sanitized before it leaves (WCR-15): marked would pass a
  // `[x](javascript:…)` href through live. Receivers must sanitize anyway;
  // this keeps our own output honest.
  return sanitizeOutgoingHtml((md.parser(tokens) as string).trim())
}

/**
 * What may begin a line inside a container: indentation (a list item) and
 * `>` markers (a quote), in any nesting.
 */
const CONTAINER_PREFIX = '(?:[ \\t]*>)*[ \\t]*'

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

/**
 * Where `raw` occurs in `text` from `cursor`. A block inside a quote or a
 * list item reaches the lexer with each later line's container prefix
 * removed, so its lines are matched with any prefix put back.
 */
function locate(
  text: string,
  raw: string,
  cursor: number,
): { start: number; end: number } | null {
  const start = text.indexOf(raw, cursor)
  if (start !== -1) return { start, end: start + raw.length }
  if (!raw.includes('\n')) return null
  const pattern = new RegExp(
    raw.split('\n').map(escapeRegExp).join(`\\n${CONTAINER_PREFIX}`),
    'g',
  )
  pattern.lastIndex = cursor
  const match = pattern.exec(text)
  return match === null
    ? null
    : { start: match.index, end: match.index + match[0].length }
}

/**
 * Where `text`'s Markdown code is: fenced and indented blocks, and code
 * spans, as ranges of `text`, including code inside quotes and list items.
 * Found from the same lexer the send uses, so it agrees with what renders as
 * code. Callers use this to leave code alone, never to find it: a range it
 * cannot place is left out.
 */
export function markdownCodeRanges(
  text: string,
): { start: number; end: number }[] {
  const ranges: { start: number; end: number }[] = []
  let cursor = 0
  md.walkTokens(md.lexer(text), (token) => {
    if (token.type !== 'code' && token.type !== 'codespan') return
    const range = locate(text, token.raw, cursor)
    if (range === null) return
    ranges.push(range)
    cursor = range.end
  })
  return ranges
}

/** Convert Markdown input to compact, display-only text for one-line previews. */
export function markdownToPlainText(text: string): string {
  const html = md.parser(md.lexer(text)) as string
  const separated = html.replace(BLOCK_END, '$& ')
  const parsed = new DOMParser().parseFromString(separated, 'text/html')
  return (parsed.body.textContent ?? '').replace(/\s+/g, ' ').trim()
}
