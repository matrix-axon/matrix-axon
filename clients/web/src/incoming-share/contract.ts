import type { components } from '../api/schema'
import { matchUrls } from '../html/linkify'

export type PreviewInput = components['schemas']['LinkPreviewRequest']
export type PreviewFailure = 'invalid' | 'preview_too_large' | 'event_too_large'
const bytes = (value: unknown) =>
  new TextEncoder().encode(JSON.stringify(value)).length

/** Mirrors the server's preview-aware projection, including both spellings
 * and its bounded image/media reservations. The portable server fixtures are
 * run against this and Swift; wire schema drift is checked separately. */
export function previewPreflight(
  request: components['schemas']['SendMessageRequest'],
  media = false,
  report?: (field: string, measured: number, limit: number) => void,
): PreviewFailure | null {
  const previews = request.url_previews
  if (previews == null) return null
  if (!Array.isArray(previews)) return 'invalid'
  if (previews.length > 8) {
    report?.('Preview count', previews.length, 8)
    return 'invalid'
  }
  if (new TextEncoder().encode(request.body).length > 64 * 1024)
    return 'event_too_large'
  const urls = new Set(matchUrls(request.body).map((match) => match.url))
  const seen = new Set<string>()
  const entries: Record<string, unknown>[] = []
  for (const preview of previews) {
    if (preview === null || typeof preview !== 'object') return 'invalid'
    if (
      Object.keys(preview).some(
        (key) =>
          ![
            'matched_url',
            'title',
            'description',
            'site_name',
            'image_upload_id',
          ].includes(key),
      )
    )
      return 'invalid'
    const raw = preview.matched_url
    if (
      typeof raw !== 'string' ||
      raw.length === 0 ||
      raw.length > 4096 ||
      !urls.has(raw)
    )
      return 'invalid'
    let url: URL
    try {
      url = new URL(raw)
    } catch {
      return 'invalid'
    }
    if (
      !['http:', 'https:'].includes(url.protocol) ||
      url.username !== '' ||
      url.password !== '' ||
      seen.has(url.href)
    )
      return 'invalid'
    seen.add(url.href)
    const entry: Record<string, unknown> = { 'matrix:matched_url': raw }
    for (const [key, limit] of [
      ['title', 300],
      ['description', 1000],
      ['site_name', 120],
    ] as const) {
      const value = preview[key]
      if (value == null) continue
      if (typeof value !== 'string') return 'invalid'
      if (value.length > limit) {
        report?.(`${key} UTF-16 code units`, value.length, limit)
        return 'invalid'
      }
      entry[`og:${key}`] = value
    }
    if (preview.image_upload_id != null) {
      if (
        !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(
          preview.image_upload_id,
        )
      )
        return 'invalid'
      entry['matrix:image:encrypted'] = 'x'.repeat(1280)
    }
    entries.push(entry)
  }
  const previewBytes = bytes(entries)
  if (previewBytes > 32 * 1024) {
    report?.('Canonical preview UTF-8 bytes', previewBytes, 32 * 1024)
    return 'preview_too_large'
  }
  const content: Record<string, unknown> = {
    msgtype: 'm.text',
    body: request.body,
  }
  if (
    (request.format == null) !== (request.formatted_body == null) ||
    (request.format != null && request.format !== 'org.matrix.custom.html')
  )
    return 'invalid'
  if (request.formatted_body != null)
    Object.assign(content, {
      format: request.format,
      formatted_body: request.formatted_body,
    })
  if (request.reply_to != null || request.thread_root != null) {
    const relation: Record<string, unknown> = {}
    if (request.thread_root != null)
      Object.assign(relation, {
        rel_type: 'm.thread',
        event_id: request.thread_root,
      })
    if (request.reply_to != null)
      relation['m.in_reply_to'] = { event_id: request.reply_to }
    content['m.relates_to'] = relation
  }
  content['m.url_previews'] = entries
  content['com.beeper.linkpreviews'] = entries.map((entry) => {
    const result: Record<string, unknown> = {
      ...entry,
      matched_url: entry['matrix:matched_url'],
    }
    delete result['matrix:matched_url']
    if (result['matrix:image:encrypted'] !== undefined) {
      result['beeper:image:encryption'] = result['matrix:image:encrypted']
      delete result['matrix:image:encrypted']
    }
    return result
  })
  if (media) content._reserved_media_fields = 'x'.repeat(12 * 1024)
  const contentBytes = bytes(content)
  if (contentBytes > 40 * 1024) {
    report?.('Serialized content UTF-8 bytes', contentBytes, 40 * 1024)
    return 'event_too_large'
  }
  return null
}
