import { customReactionMxc, reactionShortcode } from './custom-reactions'
import { randomId } from './random-id'
import { markdownToHtmlIfFormatted } from './markdown/markdown'

/** UTF-16 offsets, like textarea selections. Stored beside the draft's text. */
export interface InlineEmojiReference {
  start: number
  end: number
  shortcode: string
  mxc: string
}

export const MAX_INLINE_EMOJI = 64

/** The device-state API bounds each JSON value to 64 KiB. */
export function inlineEmojiDraftFits(
  text: string,
  refs: readonly InlineEmojiReference[],
): boolean {
  return (
    new TextEncoder().encode(JSON.stringify({ text, inline_emoji: refs }))
      .length <= 65_536
  )
}

/** Device-state and event metadata are untrusted, including their offsets. */
export function parseInlineEmoji(
  text: string,
  value: unknown,
): InlineEmojiReference[] {
  if (!Array.isArray(value)) return []
  const result: InlineEmojiReference[] = []
  for (const item of value.slice(0, MAX_INLINE_EMOJI)) {
    if (typeof item !== 'object' || item === null) continue
    const { start, end, shortcode, mxc } = item as Record<string, unknown>
    const name = reactionShortcode(shortcode)
    if (
      typeof start !== 'number' ||
      !Number.isSafeInteger(start) ||
      start < 0 ||
      typeof end !== 'number' ||
      !Number.isSafeInteger(end) ||
      end > text.length ||
      end <= start ||
      name === null ||
      typeof mxc !== 'string' ||
      customReactionMxc(mxc) === null ||
      text.slice(start, end) !== `:${name}:`
    )
      continue
    result.push({ start, end, shortcode: name, mxc })
  }
  result.sort((a, b) => a.start - b.start)
  let previousEnd = 0
  return result.filter((item) => {
    if (item.start < previousEnd) return false
    previousEnd = item.end
    return true
  })
}

/** Recover bindings for editing our sent messages, including repeated names.
 * Count visible occurrences after Markdown parsing, so a name in a URL's
 * destination cannot shift the image onto a different occurrence in the body.
 * Ambiguous foreign formatting is left unbound rather than guessed.
 */
export function inlineEmojiFromEvent(
  body: string,
  content: unknown,
): InlineEmojiReference[] {
  if (body.length > 131072 || typeof content !== 'object' || content === null)
    return []
  const html = (content as { formatted_body?: unknown }).formatted_body
  if (typeof html !== 'string' || html.length > 65536) return []
  const actual = document.createElement('template')
  actual.innerHTML = html
  const images = [...actual.content.querySelectorAll('img[data-mx-emoticon]')]
    .slice(0, MAX_INLINE_EMOJI)
    .flatMap((img) => {
      const alt = img.getAttribute('alt') ?? ''
      const shortcode =
        alt.startsWith(':') && alt.endsWith(':')
          ? reactionShortcode(alt.slice(1, -1))
          : null
      const src = img.getAttribute('src')
      return shortcode !== null &&
        src !== null &&
        customReactionMxc(src) !== null
        ? [{ img, shortcode, mxc: src }]
        : []
    })
  if (!images.length) return []
  const names = [...new Set(images.map((image) => image.shortcode))].sort(
    (a, b) => b.length - a.length,
  )
  const escape = (value: string) => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const namePattern = names.map((name) => escape(`:${name}:`)).join('|')
  const prefix = `AxonEdit${randomId().replace(/[^A-Za-z0-9]/g, '')}Token`
  const candidates: InlineEmojiReference[] = []
  const protectedBody = body.replace(
    new RegExp(namePattern, 'gu'),
    (token: string, start: number) => {
      const index = candidates.length
      candidates.push({
        start,
        end: start + token.length,
        shortcode: token.slice(1, -1),
        mxc: '',
      })
      return `${prefix}source${index}End`
    },
  )
  const parsed = document.createElement('template')
  const rendered = markdownToHtmlIfFormatted(protectedBody)
  if (rendered === null)
    parsed.content.append(document.createTextNode(protectedBody))
  else parsed.innerHTML = rendered
  const byName = new Map<string, InlineEmojiReference[]>()
  for (const match of (parsed.content.textContent ?? '').matchAll(
    new RegExp(`${prefix}source(\\d+)End`, 'g'),
  )) {
    const ref = candidates[Number(match[1])]
    if (!ref) continue
    const list = byName.get(ref.shortcode) ?? []
    list.push(ref)
    byName.set(ref.shortcode, list)
  }
  images.forEach((image, i) =>
    image.img.replaceWith(document.createTextNode(`${prefix}image${i}End`)),
  )
  const counts = new Map<string, number>()
  const result: InlineEmojiReference[] = []
  const pattern = new RegExp(`${prefix}image(\\d+)End|${namePattern}`, 'gu')
  for (const match of (actual.content.textContent ?? '').matchAll(pattern)) {
    const image = match[1] === undefined ? undefined : images[Number(match[1])]
    const name = image?.shortcode ?? match[0].slice(1, -1)
    const occurrence = counts.get(name) ?? 0
    const ref = byName.get(name)?.[occurrence]
    if (image && ref) result.push({ ...ref, mxc: image.mxc })
    counts.set(name, occurrence + 1)
  }
  return parseInlineEmoji(
    body,
    result.filter(
      (ref) => counts.get(ref.shortcode) === byName.get(ref.shortcode)?.length,
    ),
  )
}

/** Keep identities around an edit; editing a token itself makes it plain text. */
export function rebaseInlineEmoji(
  before: string,
  after: string,
  references: readonly InlineEmojiReference[],
  edit?: { start: number; end: number },
): InlineEmojiReference[] {
  let start = 0
  while (
    start < before.length &&
    start < after.length &&
    before[start] === after[start]
  )
    start++
  let oldEnd = before.length
  let newEnd = after.length
  while (
    oldEnd > start &&
    newEnd > start &&
    before[oldEnd - 1] === after[newEnd - 1]
  ) {
    oldEnd--
    newEnd--
  }
  if (
    edit &&
    edit.start >= 0 &&
    edit.end >= edit.start &&
    edit.end <= before.length &&
    after.length >= edit.start + before.length - edit.end &&
    after.startsWith(before.slice(0, edit.start)) &&
    after.endsWith(before.slice(edit.end))
  ) {
    start = edit.start
    oldEnd = edit.end
    newEnd = edit.end + after.length - before.length
  }
  const delta = newEnd - oldEnd
  return parseInlineEmoji(
    after,
    references.flatMap((item) => {
      if (item.end <= start) return [item]
      if (item.start >= oldEnd)
        return [{ ...item, start: item.start + delta, end: item.end + delta }]
      return []
    }),
  )
}

/** Formatting preserves the selected characters while changing delimiters. */
export function formatInlineEmoji(
  before: string,
  refs: readonly InlineEmojiReference[],
  edit: { from: number; to: number; insert: string },
): InlineEmojiReference[] {
  const after = before.slice(0, edit.from) + edit.insert + before.slice(edit.to)
  const outside = rebaseInlineEmoji(before, after, refs, {
    start: edit.from,
    end: edit.to,
  })
  const old = before.slice(edit.from, edit.to)
  const wrapped = edit.insert.indexOf(old)
  const unwrapped = old.indexOf(edit.insert)
  const offset = wrapped >= 0 ? wrapped : unwrapped >= 0 ? -unwrapped : null
  if (offset !== null) {
    for (const ref of refs) {
      if (ref.start >= edit.from && ref.end <= edit.to)
        outside.push({
          ...ref,
          start: ref.start + offset,
          end: ref.end + offset,
        })
    }
  }
  return parseInlineEmoji(after, outside)
}

/** Protect custom names from Unicode expansion and Markdown interpretation.
 * Only text nodes become images: URLs, code, and escaped tokens stay literal.
 */
export function protectInlineEmoji(
  body: string,
  references: readonly InlineEmojiReference[] = [],
) {
  const refs = parseInlineEmoji(body, references).filter((ref) => {
    let slashes = 0
    for (let i = ref.start - 1; i >= 0 && body[i] === '\\'; i--) slashes++
    return slashes % 2 === 0
  })
  let prefix = `AxonEmoji${randomId().replace(/[^A-Za-z0-9]/g, '')}Token`
  while (body.includes(prefix)) prefix += 'X'
  const tokens = refs.map((_, i) => `${prefix}${i}End`)
  let text = ''
  let last = 0
  refs.forEach((ref, i) => {
    text += body.slice(last, ref.start) + tokens[i]
    last = ref.end
  })
  text += body.slice(last)
  const plain = (value: string) => {
    tokens.forEach((token, i) => {
      value = value.replaceAll(token, `:${refs[i].shortcode}:`)
    })
    return value
  }
  const render = (html: string) => {
    if (refs.length === 0) return { html, changed: false }
    const template = document.createElement('template')
    template.innerHTML = html
    // Restore placeholders in destinations too, without ever inserting markup
    // into an attribute or letting a temporary token escape onto the wire.
    for (const element of template.content.querySelectorAll('*')) {
      for (const attr of [...element.attributes])
        element.setAttribute(attr.name, plain(attr.value))
    }
    const walker = document.createTreeWalker(
      template.content,
      NodeFilter.SHOW_TEXT,
    )
    const nodes: Text[] = []
    while (walker.nextNode()) nodes.push(walker.currentNode as Text)
    let changed = false
    for (const node of nodes) {
      const pattern = new RegExp(`${prefix}(\\d+)End`, 'g')
      const fragment = document.createDocumentFragment()
      let cursor = 0
      for (const match of node.data.matchAll(pattern)) {
        fragment.append(node.data.slice(cursor, match.index))
        const ref = refs[Number(match[1])]
        if (!ref) {
          fragment.append(match[0])
          continue
        }
        if (node.parentElement?.closest('a, code, pre')) {
          fragment.append(`:${ref.shortcode}:`)
        } else {
          const img = document.createElement('img')
          img.setAttribute('data-mx-emoticon', '')
          img.src = ref.mxc
          img.alt = `:${ref.shortcode}:`
          img.title = `:${ref.shortcode}:`
          img.height = 24
          fragment.append(img)
          changed = true
        }
        cursor = match.index + match[0].length
      }
      fragment.append(node.data.slice(cursor))
      node.replaceWith(fragment)
    }
    return { html: template.innerHTML, changed }
  }
  return { text, plain, render }
}
