import { matchUrls } from './html/linkify'

export interface LinkPreview {
  url: string
  hostname: string
  title: string | null
  description: string | null
  siteName: string | null
}

const MAX_BODY_LENGTH = 65_536
const MAX_URL_LENGTH = 4096
const MAX_ENTRIES = 32
const MAX_PREVIEWS = 8

function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null
}

function text(value: unknown, limit: number): string | null {
  if (typeof value !== 'string') return null
  return value.slice(0, limit).trim() || null
}

/**
 * MSC4095 bundled metadata only: no page, image, or homeserver requests.
 * The standard field is authoritative, including an empty or malformed value.
 * https://github.com/matrix-org/matrix-spec-proposals/blob/main/proposals/4095-bundled-url-previews.md
 */
export function parseLinkPreviews(
  content: unknown,
  body: string | null | undefined,
): LinkPreview[] {
  const c = object(content)
  if (c === null || !body || body.length > MAX_BODY_LENGTH) return []
  // Conservatively omit previews for the entire formatted message when it
  // contains spoilers, so metadata cannot expose text hidden by FormattedBody.
  if (
    c.format === 'org.matrix.custom.html' &&
    typeof c.formatted_body === 'string'
  ) {
    if (
      c.formatted_body.length > MAX_BODY_LENGTH ||
      /data-mx-spoiler/i.test(c.formatted_body)
    )
      return []
  }
  const standard = Object.hasOwn(c, 'm.url_previews')
  const entries = standard ? c['m.url_previews'] : c['com.beeper.linkpreviews']
  if (!Array.isArray(entries) || entries.length === 0) return []
  const bodyUrls = new Set(matchUrls(body).map(({ url }) => url))
  const seen = new Set<string>()
  const previews: LinkPreview[] = []
  for (const raw of entries.slice(0, MAX_ENTRIES)) {
    const entry = object(raw)
    if (entry === null) continue
    const matched = entry[standard ? 'matrix:matched_url' : 'matched_url']
    if (
      typeof matched !== 'string' ||
      matched.length > MAX_URL_LENGTH ||
      !bodyUrls.has(matched)
    )
      continue
    let url: URL
    try {
      url = new URL(matched)
    } catch {
      continue
    }
    if (
      !['http:', 'https:'].includes(url.protocol) ||
      url.username ||
      url.password ||
      seen.has(url.href)
    )
      continue
    const title = text(entry['og:title'], 300)
    const description = text(entry['og:description'], 1000)
    const siteName = text(entry['og:site_name'], 120)
    // A whitelist-only entry needs preview generation, outside this renderer.
    if (title === null && description === null && siteName === null) continue
    seen.add(url.href)
    previews.push({
      url: url.href,
      hostname: url.hostname,
      title,
      description,
      siteName,
    })
    if (previews.length === MAX_PREVIEWS) break
  }
  return previews
}
