import type { components } from './api/schema'
import { matchUrls } from './html/linkify'
import { previewPreflight, type PreviewInput } from './incoming-share/contract'
import { linkIdentity, nativePreviewLinks } from './preview-links'

/**
 * What a composer send carries in `url_previews` (ADR 0132 § 6), decided
 * against the exact request about to go out.
 */

/** A thumbnail as bytes. A send uploads it afresh on every attempt (§ 7). */
export interface PreviewThumbnail {
  bytes: Uint8Array<ArrayBuffer>
  mimeType: 'image/jpeg' | 'image/png' | 'image/webp'
}

/** One card's content, normalized with `previewField`. */
export interface PreviewContent {
  title: string | null
  description: string | null
  siteName: string | null
  image: PreviewThumbnail | null
}

/** A preview a send carries, keyed by the link's exact text in the body. */
export interface OutgoingLinkPreview extends PreviewContent {
  matchedUrl: string
}

/** What the composer hands its send: the cards the user kept, and the links
 *  they discarded, by identity, for the rest of the draft. */
export interface PreviewDraft {
  /** Ready, kept cards in text order, at most three. */
  cards: readonly OutgoingLinkPreview[]
  discarded: ReadonlySet<string>
}

export type PreviewPlan =
  /** Leave `url_previews` out: recipients' clients may preview on their own. */
  | { kind: 'omit' }
  /** Send these cards; an empty list is the "no previews" marker. */
  | { kind: 'send'; previews: OutgoingLinkPreview[] }
  /**
   * Even `[]` is too large, and the body holds a link the user discarded, so
   * omitting the field would expose it. The user decides (§ 6).
   */
  | { kind: 'too-long-for-marker' }

type MessageRequest = Pick<
  components['schemas']['SendMessageRequest'],
  'body' | 'format' | 'formatted_body' | 'reply_to' | 'thread_root'
>

/** Stands in for an upload id that does not exist until the send uploads. */
const PLACEHOLDER_UPLOAD_ID = '00000000-0000-4000-8000-000000000001'

/** The wire entry for one preview. Absent fields are left out, as the
 *  preflight measures them. */
export function wirePreview(
  preview: OutgoingLinkPreview,
  imageUploadId: string | null,
): PreviewInput {
  return {
    matched_url: preview.matchedUrl,
    ...(preview.title !== null && { title: preview.title }),
    ...(preview.description !== null && { description: preview.description }),
    ...(preview.siteName !== null && { site_name: preview.siteName }),
    ...(imageUploadId !== null && { image_upload_id: imageUploadId }),
  }
}

/**
 * Decide the field for one send. The body is the formatted one the server
 * matches against, so a card counts only while its exact link text is still
 * a complete bare link there. The first matching rule wins:
 *
 * 1. no draft (previews off, an edit, attachments) or a spoiler: omit;
 * 2. a kept card that fits, after dropping images and then cards from the
 *    end until `previewPreflight` passes: send the cards;
 * 3. any eligible link in the body: send `[]`, unless even that is too
 *    large, which omits the field when no discarded link is in the body and
 *    otherwise asks the user (`allowUnmarked` is their answer);
 * 4. otherwise omit.
 */
export function planLinkPreviews(
  request: MessageRequest,
  draft: PreviewDraft | undefined,
  allowUnmarked = false,
): PreviewPlan {
  if (
    draft === undefined ||
    /data-mx-spoiler/i.test(request.formatted_body ?? '')
  )
    return { kind: 'omit' }
  const eligible = nativePreviewLinks(request.body)
  const bare = new Set(matchUrls(request.body).map(({ url }) => url))
  const counted = new Set<string>()
  let previews = draft.cards.filter((card) => {
    const identity = linkIdentity(card.matchedUrl)
    if (
      identity === null ||
      counted.has(identity) ||
      !bare.has(card.matchedUrl) ||
      !eligible.includes(card.matchedUrl)
    )
      return false
    counted.add(identity)
    return true
  })
  const fits = (list: readonly OutgoingLinkPreview[]) =>
    previewPreflight({
      ...request,
      url_previews: list.map((preview) =>
        wirePreview(
          preview,
          preview.image === null ? null : PLACEHOLDER_UPLOAD_ID,
        ),
      ),
    }) === null
  while (previews.length > 0 && !fits(previews)) {
    const last = previews.findLastIndex((preview) => preview.image !== null)
    previews =
      last === -1
        ? previews.slice(0, -1)
        : previews.map((preview, index) =>
            index === last ? { ...preview, image: null } : preview,
          )
  }
  if (previews.length > 0) return { kind: 'send', previews }
  if (eligible.length === 0) return { kind: 'omit' }
  if (fits([])) return { kind: 'send', previews: [] }
  const discardedInBody = eligible.some((url) => {
    const identity = linkIdentity(url)
    return identity !== null && draft.discarded.has(identity)
  })
  return discardedInBody && !allowUnmarked
    ? { kind: 'too-long-for-marker' }
    : { kind: 'omit' }
}

/**
 * Whether a failed send that carried `url_previews` was rejected for them
 * (§ 6's table). These are definite failures: Retry then sends `[]`.
 */
export function isPreviewRejection(
  status: number | undefined,
  code: unknown,
): boolean {
  return (
    (status === 413 &&
      (code === 'preview_too_large' ||
        code === 'event_too_large' ||
        code === 'payload_too_large')) ||
    (status === 400 && code === 'bad_request') ||
    (status === 404 && code === 'not_found')
  )
}
