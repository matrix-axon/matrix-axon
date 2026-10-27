import { matchUrls } from './html/linkify'
import { mxcToPath } from './media/parse-media'

/** No keys or arbitrary source fields escape the event parser. */
export interface LinkPreviewImage {
  mxc: string
  encrypted: boolean
}

export interface LinkPreview {
  url: string
  hostname: string
  title: string | null
  description: string | null
  siteName: string | null
  image: LinkPreviewImage | null
}

export const MAX_PREVIEW_IMAGE_BYTES = 2 * 1024 * 1024

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

function imageMxc(value: unknown): string | null {
  if (typeof value !== 'string' || value.length > 2048) return null
  const path = mxcToPath(value)
  return path !== null &&
    !/[\s\p{Cc}?#\\]/u.test(value) &&
    !path.serverName.includes('@') &&
    !path.mediaId.includes('/') &&
    new TextEncoder().encode(value).length <= 2048
    ? value
    : null
}

function previewImage(entry: Record<string, unknown>): LinkPreviewImage | null {
  // Sender dimensions never size the DOM. Reject implausible metadata before
  // starting media work; missing dimensions still use the fixed preview slot.
  const width = entry['og:image:width']
  const height = entry['og:image:height']
  for (const dimension of [width, height]) {
    if (
      dimension !== undefined &&
      (typeof dimension !== 'number' ||
        !Number.isSafeInteger(dimension) ||
        dimension <= 0 ||
        dimension > 4096)
    )
      return null
  }
  if (
    typeof width === 'number' &&
    typeof height === 'number' &&
    width * height > 4_000_000
  )
    return null
  const size = entry['matrix:image:size']
  if (
    size !== undefined &&
    (typeof size !== 'number' ||
      !Number.isSafeInteger(size) ||
      size <= 0 ||
      size > MAX_PREVIEW_IMAGE_BYTES)
  )
    return null
  const type = entry['og:image:type']
  if (
    type !== undefined &&
    !['image/png', 'image/jpeg', 'image/webp'].includes(type as string)
  )
    return null

  // Descriptor presence is authoritative: a malformed encrypted descriptor
  // must not fall back to a plaintext alias. Decryption stays on the server.
  const encryptedKey = Object.hasOwn(entry, 'matrix:image:encrypted')
    ? 'matrix:image:encrypted'
    : Object.hasOwn(entry, 'beeper:image:encryption')
      ? 'beeper:image:encryption'
      : null
  if (encryptedKey !== null) {
    const file = object(entry[encryptedKey])
    const key = object(file?.key)
    const hashes = object(file?.hashes)
    const mxc = imageMxc(file?.url)
    if (
      mxc === null ||
      file?.v !== 'v2' ||
      key?.kty !== 'oct' ||
      key.alg !== 'A256CTR' ||
      key.ext !== true ||
      typeof key.k !== 'string' ||
      !/^[A-Za-z0-9_-]{43}$/.test(key.k) ||
      !Array.isArray(key.key_ops) ||
      key.key_ops.length > 2 ||
      !key.key_ops.includes('decrypt') ||
      !key.key_ops.every(
        (operation) => operation === 'encrypt' || operation === 'decrypt',
      ) ||
      typeof file.iv !== 'string' ||
      !/^[A-Za-z0-9+/]{22}$/.test(file.iv) ||
      typeof hashes?.sha256 !== 'string' ||
      !/^[A-Za-z0-9+/]{43}$/.test(hashes.sha256)
    )
      return null
    return { mxc, encrypted: true }
  }
  const mxc = imageMxc(entry['og:image'])
  return mxc === null ? null : { mxc, encrypted: false }
}

/**
 * Shared normalized MSC4095 shape for timeline and future pending-share UI.
 * Parsing never performs I/O; images carry only an eligible Matrix reference.
 * The standard field is authoritative, including an empty or malformed value.
 * https://github.com/matrix-org/matrix-spec-proposals/blob/main/proposals/4095-bundled-url-previews.md
 */
export function parseLinkPreviews(
  content: unknown,
  body: string | null | undefined,
): LinkPreview[] {
  const c = object(content)
  if (c === null || !body || body.length > MAX_BODY_LENGTH) return []
  const standard = Object.hasOwn(c, 'm.url_previews')
  const entries = standard ? c['m.url_previews'] : c['com.beeper.linkpreviews']
  if (!Array.isArray(entries) || entries.length === 0) return []
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
    const image = previewImage(entry)
    // A whitelist-only entry needs preview generation, outside this renderer.
    if (
      title === null &&
      description === null &&
      siteName === null &&
      image === null
    )
      continue
    seen.add(url.href)
    previews.push({
      url: url.href,
      hostname: url.hostname,
      title,
      description,
      siteName,
      image,
    })
    if (previews.length === MAX_PREVIEWS) break
  }
  return previews
}
