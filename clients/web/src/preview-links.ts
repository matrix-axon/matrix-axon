import { matchUrls } from './html/linkify'

/**
 * The links a preview may be built for, and how two of them are compared
 * (ADR 0132 § 1). Shared by the composer and pending-share recovery, which
 * must agree on both, and mirrored natively by the shell's fetcher
 * (`src-tauri/axon-link-preview/src/url_check.rs`) and the iOS Share
 * Extension's reader. All three read
 * `incoming-share/fixtures/device-preview-urls.json`.
 */

/** TLDs that never name a public site. */
const RESERVED_SUFFIXES: ReadonlySet<string> = new Set([
  'local',
  'localhost',
  'home',
  'lan',
  'corp',
  'internal',
  'localdomain',
  'intranet',
  'private',
  'onion',
  'i2p',
  'invalid',
  'test',
  'example',
  'alt',
  'arpa',
])

/** Static URL eligibility shared with the native restricted preview reader.
 * DNS and every redirect/image address still require native validation.
 * Other URLs remain ordinary message text; ineligible metadata is optional. */
export function isPreviewLink(raw: string): boolean {
  if (
    raw.length > 4096 ||
    /%(?![0-9a-f]{2})/i.test(raw) ||
    !/^[A-Za-z0-9\-._~:/?#[\]@!$&'()*+,;=%]+$/.test(raw)
  )
    return false
  try {
    const url = new URL(raw)
    const host = url.hostname.toLowerCase().replace(/\.$/, '')
    const labels = host.split('.')
    return (
      host.length <= 253 &&
      labels.length > 1 &&
      /^[a-z0-9.-]+$/.test(host) &&
      labels.every(
        (label) =>
          label.length > 0 &&
          label.length <= 63 &&
          !label.startsWith('-') &&
          !label.endsWith('-') &&
          !label.startsWith('xn--'),
      ) &&
      !/^\d+$/.test(labels.at(-1)!) &&
      !RESERVED_SUFFIXES.has(labels.at(-1)!) &&
      (url.port === '' ||
        (Number(url.port) >= 1 && Number(url.port) <= 65535)) &&
      url.protocol === 'https:' &&
      url.hostname !== '' &&
      url.username === '' &&
      url.password === ''
    )
  } catch {
    return false
  }
}

/** Every bare link in `text` that passes {@link isPreviewLink}, in text order. */
export function nativePreviewLinks(text: string): string[] {
  return matchUrls(text)
    .map(({ url }) => url)
    .filter(isPreviewLink)
}

/**
 * The identity two links share when they name one destination: the WHATWG
 * serialization, which is what the server's `url` crate compares when it
 * rejects a send naming one destination twice. `null` for text that does
 * not parse. The cases are in `fixtures/link-identity.json`, which the
 * server's own test reads too.
 */
export function linkIdentity(raw: string): string | null {
  try {
    return new URL(raw).href
  } catch {
    return null
  }
}

/** Send-contract field limits, in UTF-16 code units, which `length` counts. */
export const PREVIEW_FIELD_LIMITS = {
  title: 300,
  description: 1000,
  siteName: 120,
} as const

/**
 * Foundation's `whitespacesAndNewlines`: Unicode `White_Space` plus U+200B,
 * which `foundation_trim` in the plugin's `page.rs` measured on macOS.
 */
const EDGE_SPACE = /^[\p{White_Space}\u200B]+|[\p{White_Space}\u200B]+$/gu

/**
 * `TextShare.previewField`, as the plugin's `preview_field` ports it: a
 * value longer than `limit`, before or after trimming, or empty after it, is
 * dropped, never truncated. The composer applies it to every card, from
 * either source, so an out-of-range field never reaches the server.
 */
export function previewField(value: unknown, limit: number): string | null {
  if (typeof value !== 'string' || value.length > limit) return null
  const trimmed = value.replace(EDGE_SPACE, '')
  return trimmed === '' || trimmed.length > limit ? null : trimmed
}
