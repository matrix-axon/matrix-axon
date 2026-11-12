/**
 * Bare-URL detection shared by both rendering paths: plain-text bodies
 * (VNode anchors in `FormattedBody`) and formatted bodies (DOM anchors in
 * `sanitize.ts`). Many clients — Element included — send bare URLs as plain
 * text even inside `formatted_body` and rely on the receiver to linkify.
 */

/**
 * A bare http(s) or matrix: URL; trailing sentence punctuation is trimmed
 * after. The http(s) scheme matches in any case, as RFC 3986 defines it,
 * because a keyboard that capitalizes a sentence's first letter turns
 * `https://` into `Https://`. `matrix:` stays lowercase.
 */
const URL_PATTERN = /(?:[hH][tT][tT][pP][sS]?:\/\/|matrix:)[^\s<>"]+/g
/** A scheme only, as `URL_PATTERN` matched it. */
const HTTP_SCHEME = /^https?:/i
/** Punctuation that reads as sentence structure, not part of the URL. */
const TRAILING_PUNCTUATION = /[.,;:!?)\]'"]+$/

export interface UrlMatch {
  url: string
  start: number
  /** End of the URL itself (trailing punctuation excluded). */
  end: number
}

/** All bare supported URLs in `text`, with sentence punctuation trimmed. */
export function matchUrls(text: string): UrlMatch[] {
  const matches: UrlMatch[] = []
  for (const match of text.matchAll(URL_PATTERN)) {
    let url = match[0]
    const trimmed = TRAILING_PUNCTUATION.exec(url)
    if (trimmed !== null) {
      url = url.slice(0, -trimmed[0].length)
    }
    const bare = url.toLowerCase()
    if (bare === 'http://' || bare === 'https://' || url === 'matrix:') {
      continue
    }
    matches.push({ url, start: match.index, end: match.index + url.length })
  }
  return matches
}

/**
 * The bare URLs axon's server finds in a body (`body_urls` in
 * `crates/axon-core/src/link_preview.rs`), which a preview's `matched_url`
 * must be one of: only lowercase `http://` and `https://` schemes. The
 * composer lowercases schemes before sending (`lowercaseLinkSchemes`), so its
 * links always qualify; this is for the checks that mirror the server.
 */
export function serverBodyUrls(text: string): Set<string> {
  return new Set(
    matchUrls(text)
      .map(({ url }) => url)
      .filter((url) => url.startsWith('http://') || url.startsWith('https://')),
  )
}

/**
 * `text` with the scheme of each bare http(s) link lowercased, and nothing
 * else changed: the same length, so offsets into it (inline emoji, mentions)
 * still hold. Links inside `code` are left as written. Applied to what the
 * composer sends, so the server matches the links it previews, and every
 * recipient's client linkifies them.
 */
export function lowercaseLinkSchemes(
  text: string,
  code: readonly { start: number; end: number }[] = [],
): string {
  let result = text
  for (const { url, start } of matchUrls(text)) {
    const scheme = HTTP_SCHEME.exec(url)?.[0]
    if (
      scheme === undefined ||
      scheme === scheme.toLowerCase() ||
      code.some((range) => start >= range.start && start < range.end)
    )
      continue
    result =
      result.slice(0, start) +
      scheme.toLowerCase() +
      result.slice(start + scheme.length)
  }
  return result
}

/**
 * Linkify bare URLs in the text nodes under `root`, in place. Text already
 * inside an anchor, code, or preformatted block is left alone (no nested
 * links, no rewriting code samples).
 */
export function linkifyDomTree(root: Element): void {
  const doc = root.ownerDocument
  const walker = doc.createTreeWalker(root, NodeFilter.SHOW_TEXT)
  const textNodes: Text[] = []
  let node = walker.nextNode()
  while (node !== null) {
    textNodes.push(node as Text)
    node = walker.nextNode()
  }
  for (const textNode of textNodes) {
    if (textNode.parentElement?.closest('a, code, pre') != null) {
      continue
    }
    const text = textNode.data
    const matches = matchUrls(text)
    if (matches.length === 0) {
      continue
    }
    const fragment = doc.createDocumentFragment()
    let last = 0
    for (const { url, start, end } of matches) {
      if (start > last) {
        fragment.append(text.slice(last, start))
      }
      const anchor = doc.createElement('a')
      anchor.setAttribute('href', url)
      anchor.setAttribute('target', '_blank')
      anchor.setAttribute('rel', 'noopener noreferrer')
      anchor.textContent = url
      fragment.append(anchor)
      last = end
    }
    if (last < text.length) {
      fragment.append(text.slice(last))
    }
    textNode.replaceWith(fragment)
  }
}
