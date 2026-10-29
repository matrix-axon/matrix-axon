import DOMPurify from 'dompurify'
import { linkifyDomTree } from './linkify'
import { customReactionMxc } from '../custom-reactions'

interface MatrixHtmlRenderContext {
  resolveUserLink?: (
    href: string,
    label: string,
  ) => { href: string; label: string; userId?: string } | null
  resolveRoomLink?: (
    href: string,
    label: string,
  ) => {
    href: string
    label: string
    isEventLink: boolean
    action: 'open' | 'join'
  } | null
}

/**
 * Sanitized rendering of Matrix `formatted_body` HTML (ADR 0046, M-W5).
 *
 * The allowlist mirrors the Matrix client-server spec's suggested HTML subset
 * — the same subset the TUI renders via ruma's sanitizer
 * (`clients/tui/src/html.rs`) — including the `data-mx-color` /
 * `data-mx-bg-color` / `data-mx-spoiler` span attributes. Deviations:
 *
 * - `<img>` is admitted (M-W8, ADR 0064), but only for `mxc://` media: the
 *   `uponSanitizeElement` hook below copies a safe `mxc://` src to `data-mxc`
 *   and then drops `src` entirely, so an `http(s)` src — a tracking pixel we
 *   have no proxy for — never survives. `FormattedBody` resolves `data-mxc`
 *   to an authenticated blob URL after mount; an unresolved image falls back
 *   to the browser's native `alt` rendering.
 * - `<mx-reply>` rich-reply fallbacks are removed *with their contents*
 *   (`RemoveReplyFallback::Yes` in the TUI): the reply context is rendered
 *   from the relation, not the embedded fallback HTML.
 */
const ALLOWED_TAGS = [
  'del',
  'h1',
  'h2',
  'h3',
  'h4',
  'h5',
  'h6',
  'blockquote',
  'p',
  'a',
  'ul',
  'ol',
  'sup',
  'sub',
  'li',
  'b',
  'i',
  'u',
  'strong',
  'em',
  's',
  'code',
  'hr',
  'br',
  'div',
  'table',
  'thead',
  'tbody',
  'tr',
  'th',
  'td',
  'caption',
  'pre',
  'span',
  'font',
  'details',
  'summary',
  'img',
]

const ALLOWED_ATTR = [
  'href',
  'name',
  'target',
  'start',
  'class',
  // The legacy `<font color>` attribute is spec-allowed alongside the
  // data-mx-* pair; Element's /rainbow emits it per character. The TUI never
  // sees it because ruma's sanitizer normalizes it into data-mx-color; here
  // the transform below maps it to an inline style the same way.
  'color',
  'data-mx-color',
  'data-mx-bg-color',
  'data-mx-spoiler',
  'data-mx-emoticon',
  'title',
  // `<img>` (mxc-only): `src` is never allowed through — the hook rewrites a
  // safe `mxc://` src into `data-mxc`, which `FormattedBody` resolves.
  'alt',
  'data-mxc',
  'width',
  'height',
]

/** CSS-safe color: `#rgb`/`#rrggbb` only, the format `data-mx-color` carries. */
const HEX_COLOR = /^#(?:[0-9a-f]{3}|[0-9a-f]{6})$/i

/** The class the spoiler transform emits; the UI toggles `spoiler-revealed`. */
export const SPOILER_CLASS = 'spoiler'
const DANGEROUS_LINK_CLASS = 'dangerous-link'
const KNOWN_DISPLAY_LINK_SCHEMES = new Set([
  'http',
  'https',
  'ftp',
  'mailto',
  'matrix',
  'magnet',
])
const ALLOWED_URI_REGEXP =
  /^(?:(?:https?|mailto|ftp|tel|callto|sms|cid|xmpp|matrix|magnet):|[^a-z]|[a-z+.-]+(?:[^a-z+.\-:]|$))/i

let configured = false

function configure(): void {
  if (configured) {
    return
  }
  configured = true
  // For an `<img>`, move a safe `mxc://` src into `data-mxc` and always drop
  // `src`. This hook runs before per-attribute filtering, so the mxc value
  // survives in the allowlisted `data-mxc` while `src` — not allowlisted — is
  // dropped regardless; an `http(s)` src simply never lands anywhere, closing
  // the tracking-pixel vector. `FormattedBody` resolves `data-mxc` after mount.
  DOMPurify.addHook('uponSanitizeElement', (node, data) => {
    if (data.tagName !== 'img') {
      return
    }
    const el = node as Element
    const src = el.getAttribute?.('src')
    if (src !== null && src !== undefined && src.startsWith('mxc://')) {
      el.setAttribute('data-mxc', src)
    }
    el.removeAttribute?.('src')
  })
}

/**
 * Sanitize HTML this client is about to *send* as a `formatted_body`
 * (markdown-on-send; WCR-15). Same allowlist as the render path but none of
 * the display transforms — no spoiler classes, no `target=_blank`, no inline
 * styles — because those are receiver-side concerns. What it does close:
 * marked passes a `[x](javascript:…)` link through as a live href, and while
 * every receiving client must sanitize anyway (ours does), our own output
 * stays honest by never carrying an active scheme.
 */
export function sanitizeOutgoingHtml(
  html: string,
  { inlineEmoji = false }: { inlineEmoji?: boolean } = {},
): string {
  return outgoingPurify.sanitize(html, {
    ALLOWED_TAGS,
    ALLOWED_ATTR: inlineEmoji ? [...ALLOWED_ATTR, 'src'] : ALLOWED_ATTR,
    FORBID_CONTENTS: ['mx-reply', 'style', 'script'],
    ALLOW_DATA_ATTR: false,
    // Only generated emoticon src attributes get this exception. The hook
    // validates each image; links always retain the ordinary URI allowlist.
    ADD_URI_SAFE_ATTR: inlineEmoji ? ['src'] : [],
    ALLOWED_URI_REGEXP,
  })
}

// Display sanitization rewrites MXCs to data-mxc. Outgoing HTML must retain
// real MXC src attributes so other clients and the server can resolve them.
const outgoingPurify = DOMPurify(window)
outgoingPurify.addHook('uponSanitizeElement', (node, data) => {
  if (data.tagName !== 'img') return
  const img = node as Element
  const src = img.getAttribute('src')
  if (
    !img.hasAttribute('data-mx-emoticon') ||
    src === null ||
    customReactionMxc(src) === null
  )
    img.removeAttribute('src')
  img.removeAttribute('data-mxc')
})

/**
 * Sanitize Matrix HTML and apply the Matrix-specific transforms, returning
 * markup safe for `dangerouslySetInnerHTML`:
 *
 * - `data-mx-color` / `data-mx-bg-color` become inline `style` colors
 *   (validated as hex, exactly what the attributes are specced to carry);
 * - `data-mx-spoiler` spans get the `spoiler` class (click-to-reveal is the
 *   rendering component's job) with the reason kept as `title`;
 * - links open in a new tab with `rel="noopener noreferrer"`;
 * - `code` keeps only spec-shaped `language-*` classes; other classes drop.
 */
export function renderMatrixHtml(
  html: string,
  context: MatrixHtmlRenderContext = {},
): string {
  configure()
  const clean = DOMPurify.sanitize(html, {
    ALLOWED_TAGS,
    ALLOWED_ATTR,
    // Drop the rich-reply fallback subtree entirely, not just the tag.
    FORBID_CONTENTS: ['mx-reply', 'style', 'script'],
    ALLOW_DATA_ATTR: false,
    // Without this, DOMPurify drops custom-attribute *values* that look like
    // they carry a URI scheme — e.g. a spoiler reason of "CW: ending".
    // These attributes are never dereferenced, so any value is safe.
    ADD_URI_SAFE_ATTR: [
      'data-mx-spoiler',
      'data-mx-color',
      'data-mx-bg-color',
      // `data-mxc` carries an `mxc://` value that looks like a URI scheme;
      // without this DOMPurify strips it. It is never dereferenced by the
      // browser, only read by JS to fetch through the authenticated proxy.
      'data-mxc',
    ],
    RETURN_DOM_FRAGMENT: true,
    ALLOWED_URI_REGEXP,
  })

  const scratch = document.createElement('div')
  scratch.append(clean)

  for (const el of [...scratch.querySelectorAll('[data-mx-color]')]) {
    const color = el.getAttribute('data-mx-color')
    if (color !== null && HEX_COLOR.test(color)) {
      ;(el as HTMLElement).style.color = color
    }
  }
  // Legacy form of the same thing (Element's /rainbow). `data-mx-color`
  // wins when both are present, hence the :not() exclusion.
  for (const el of [
    ...scratch.querySelectorAll('font[color]:not([data-mx-color])'),
  ]) {
    const color = el.getAttribute('color')
    if (color !== null && HEX_COLOR.test(color)) {
      ;(el as HTMLElement).style.color = color
    }
  }
  for (const el of [...scratch.querySelectorAll('[data-mx-bg-color]')]) {
    const color = el.getAttribute('data-mx-bg-color')
    if (color !== null && HEX_COLOR.test(color)) {
      ;(el as HTMLElement).style.backgroundColor = color
    }
  }
  const spoilers = new Set<Element>()
  for (const el of [...scratch.querySelectorAll('[data-mx-spoiler]')]) {
    spoilers.add(el)
    el.classList.add(SPOILER_CLASS)
    const reason = el.getAttribute('data-mx-spoiler')
    if (reason !== null && reason !== '') {
      el.setAttribute('title', reason)
    }
    // Reachable + announced for keyboard/screen-reader users.
    el.setAttribute('tabindex', '0')
    el.setAttribute('role', 'button')
    el.setAttribute(
      'aria-label',
      reason !== null && reason !== '' ? `spoiler: ${reason}` : 'spoiler',
    )
  }
  rewriteMatrixToLinks(scratch, context)
  for (const el of [...scratch.querySelectorAll('a[href]')]) {
    if (isRoomPillLink(el) || isMentionPillLink(el)) {
      el.removeAttribute('target')
      el.removeAttribute('rel')
      continue
    }
    el.setAttribute('target', '_blank')
    el.setAttribute('rel', 'noopener noreferrer')
  }
  for (const el of [...scratch.querySelectorAll('code[class]')]) {
    const language = [...el.classList].find((cls) =>
      /^language-[\w-]+$/.test(cls),
    )
    el.removeAttribute('class')
    if (language !== undefined) {
      el.classList.add(language)
    }
  }
  // `class` is allowed through sanitization only so `code` can keep its
  // language; strip it everywhere else so senders can't borrow app styles.
  // Only classes *we* just added (the spoiler transform) survive — a sender
  // writing `class="spoiler"` directly does not get the treatment.
  for (const el of [...scratch.querySelectorAll('[class]')]) {
    if (el.tagName === 'CODE') {
      continue
    }
    if (isMentionPillLink(el)) {
      el.setAttribute('class', 'mention-pill')
      continue
    }
    if (isRoomPillLink(el)) {
      el.setAttribute('class', roomPillClass(false, isEventPillLink(el)))
      el.setAttribute('title', roomPillTitle(false, isEventPillLink(el)))
      continue
    }
    if (spoilers.has(el)) {
      el.setAttribute('class', SPOILER_CLASS)
    } else {
      el.removeAttribute('class')
    }
  }

  // Bare URLs left as text (Element sends them this way inside formatted
  // bodies and expects receivers to linkify) become anchors; text inside
  // a/code/pre is left alone.
  linkifyDomTree(scratch)
  // Intentional second pass: the class-strip loop above removes sender-owned
  // app classes, including any `room-pill join-pill` on non-local matrix:
  // anchors. After linkifying bare matrix: text, re-resolve every Matrix link
  // so known-room pills and unknown-room join pills are app-authored again.
  rewriteMatrixToLinks(scratch, context)
  markDangerousLinks(scratch)

  return scratch.innerHTML
}

function markDangerousLinks(root: Element): void {
  for (const el of [...root.querySelectorAll('a[href]')]) {
    if (
      el.classList.contains('mention-pill') ||
      el.classList.contains('room-pill')
    ) {
      continue
    }
    const href = el.getAttribute('href')?.trim() ?? ''
    const visibleText = (el.textContent ?? '').trim()
    if (
      href === '' ||
      visibleText === '' ||
      visibleText === href ||
      !looksLikeHyperlink(visibleText)
    ) {
      continue
    }
    el.classList.add(DANGEROUS_LINK_CLASS)
    el.setAttribute(
      'title',
      `Warning: link text looks like ${visibleText}, but opens ${href}`,
    )
    el.setAttribute('aria-label', `${visibleText} (warning: opens ${href})`)
  }
}

function looksLikeHyperlink(text: string): boolean {
  const trimmed = text.trim()
  const schemeEnd = trimmed.indexOf(':')
  if (schemeEnd <= 0 || schemeEnd === trimmed.length - 1) {
    return false
  }
  const scheme = trimmed.slice(0, schemeEnd).toLowerCase()
  return KNOWN_DISPLAY_LINK_SCHEMES.has(scheme)
}

function rewriteMatrixToLinks(
  root: Element,
  context: MatrixHtmlRenderContext,
): void {
  if (
    context.resolveUserLink === undefined &&
    context.resolveRoomLink === undefined
  ) {
    return
  }
  for (const el of [...root.querySelectorAll('a[href]')]) {
    const href = el.getAttribute('href')
    if (href === null) {
      continue
    }
    const label = el.textContent ?? ''
    const userLink = context.resolveUserLink?.(href, label) ?? null
    if (userLink !== null) {
      el.setAttribute('href', userLink.href)
      el.setAttribute('class', 'mention-pill')
      if (userLink.userId !== undefined) {
        el.setAttribute('title', `Open DM with ${userLink.userId}`)
      }
      el.removeAttribute('target')
      el.removeAttribute('rel')
      el.textContent = userLink.label
      continue
    }
    const roomLink = context.resolveRoomLink?.(href, label) ?? null
    if (roomLink !== null) {
      el.setAttribute('href', roomLink.href)
      el.setAttribute(
        'class',
        roomPillClass(roomLink.action === 'join', roomLink.isEventLink),
      )
      el.setAttribute(
        'title',
        roomPillTitle(roomLink.action === 'join', roomLink.isEventLink),
      )
      el.removeAttribute('target')
      el.removeAttribute('rel')
      el.textContent = roomLink.label
    }
  }
}

function roomPillClass(join: boolean, event: boolean): string {
  return ['room-pill', join ? 'join-pill' : '', event ? 'event-pill' : '']
    .filter((part) => part !== '')
    .join(' ')
}

function roomPillTitle(join: boolean, event: boolean): string {
  if (join) {
    return event ? 'Join room and jump to message' : 'Join room'
  }
  return event ? 'Open existing room at this message' : 'Open existing room'
}

function isMentionPillLink(el: Element): boolean {
  const href = el.getAttribute('href') ?? ''
  return (
    el.tagName === 'A' &&
    el.classList.contains('mention-pill') &&
    (href.startsWith('https://matrix.to/#/%40') ||
      /^\/rooms\/dm\?user=%40/.test(href))
  )
}

function isRoomPillLink(el: Element): boolean {
  const href = el.getAttribute('href') ?? ''
  return (
    el.tagName === 'A' &&
    el.classList.contains('room-pill') &&
    /^\/[^/]+\/rooms\/[^/]+/.test(href)
  )
}

function isEventPillLink(el: Element): boolean {
  const href = el.getAttribute('href') ?? ''
  return /[?&]event=/.test(href)
}
