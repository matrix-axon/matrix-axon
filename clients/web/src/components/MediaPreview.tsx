import { useEffect, useState } from 'preact/hooks'
import type { ParsedMedia } from '../media/parse-media'
import {
  previewSizeCeiling,
  type PreviewKind,
  type PreviewPlan,
} from '../media/preview-kind'
import { highlightCode } from '../code/highlight'
import { languageForFilename, languageFromShebang } from '../code/languages'
import { useMediaBlob } from '../media/use-media-blob'
import { useServices } from '../services'
import { Lightbox } from './Lightbox'
import { MediaCaption } from './MediaCaption'
import { PdfViewer } from './PdfViewer'

/** The longest side an inline video is allowed, in CSS px (matches MediaImage). */
const VIDEO_MAX = 480

/**
 * The expanded body of an inline preview (ADR 0072): a native player for
 * audio/video, the browser's own viewer for a PDF, or the decoded text of a
 * text file. Rendered only after the user asks — `MediaAttachment` mounts this
 * on expand, so nothing is fetched by merely scrolling past the card.
 *
 * The bytes come through `useMediaBlob`, so the object URL is refcounted and
 * released when this unmounts (collapsing the card). `fetchBlobUrl` would be
 * wrong here: it is uncached and caller-revoked, which suits the transient
 * Download anchor and not a player that may be re-opened.
 */
export function MediaPreview({
  accountId,
  media,
  plan,
  content,
  onClose,
}: {
  accountId: string
  media: ParsedMedia
  plan: PreviewPlan
  /** Event `content`, so a caption with `formatted_body` can use it. */
  content?: unknown
  /** Present in the lightbox and dismiss with this, instead of inline. */
  onClose?: () => void
}) {
  // Text wants a string, not an object URL, and takes a different service call
  // — so it is a sibling component rather than a branch below, keeping the
  // hooks of each path unconditional.
  return plan.kind === 'text' ? (
    <TextPreview
      accountId={accountId}
      mxcUrl={media.url!}
      filename={media.filename}
    />
  ) : (
    <BlobPreview
      accountId={accountId}
      media={media}
      kind={plan.kind}
      contentType={plan.contentType}
      content={content}
      onClose={onClose}
    />
  )
}

function BlobPreview({
  accountId,
  media,
  kind,
  contentType,
  content,
  onClose,
}: {
  accountId: string
  media: ParsedMedia
  kind: Exclude<PreviewKind, 'text'>
  contentType: string | null
  content?: unknown
  onClose?: () => void
}) {
  const [unplayable, setUnplayable] = useState(false)
  const { ref, state } = useMediaBlob<HTMLDivElement>(accountId, media.url, {
    eager: true,
    ...(contentType === null ? {} : { contentType }),
  })
  // The sender's own thumbnail, when there is one, stands in as the video
  // poster; a homeserver-generated one is not requested — the player is
  // already loading the full object.
  const poster = useMediaBlob<HTMLDivElement>(
    accountId,
    kind === 'video' ? media.thumbnailUrl : null,
    { eager: true },
  )

  const label = media.caption ?? media.filename
  const framed = onClose !== undefined

  let body
  if (state.status === 'error') {
    body = <p class="local-echo-status error">Could not load {kind}</p>
  } else if (state.status !== 'ready' || state.url === undefined) {
    body = <div class="media-skeleton" aria-hidden="true" />
  } else if (unplayable) {
    // A codec the browser cannot decode is not a load failure — the bytes are
    // here and the download works — so it gets its own message rather than the
    // generic one. `.mov` is the common case: Safari plays it, Chrome and
    // Firefox often will not.
    body = (
      <p class="local-echo-status error">
        This browser cannot play {media.filename} — use Download to open it in
        another app.
      </p>
    )
  } else if (kind === 'audio') {
    body = (
      <audio
        class="media-player"
        controls
        preload="metadata"
        src={state.url}
        onError={() => setUnplayable(true)}
      >
        <track kind="captions" />
      </audio>
    )
  } else if (kind === 'video') {
    body = (
      <video
        class={framed ? 'lightbox-player' : 'media-player'}
        controls
        autoPlay={framed}
        playsInline
        preload="metadata"
        style={framed ? undefined : videoStyle(media)}
        poster={poster.state.status === 'ready' ? poster.state.url : undefined}
        src={state.url}
        onError={() => setUnplayable(true)}
      >
        <track kind="captions" />
      </video>
    )
  } else {
    // Drawn with pdf.js rather than embedded in an `<iframe>`: iOS Safari
    // renders only the *first page* of a framed PDF, with no scrolling and no
    // controls, which made a multi-page document unreadable on a phone. Both
    // presentations use the viewer so there is one code path and one set of
    // behaviours to reason about.
    //
    // The forced `application/pdf` blob type still does the security work it
    // did for the frame: pdf.js parses the bytes as a document, so an
    // attachment whose bytes are really HTML fails to parse rather than
    // rendering as markup in our origin.
    body = (
      <div class={framed ? 'lightbox-pdf' : 'media-pdf'}>
        <PdfViewer url={state.url} label={label} />
      </div>
    )
  }

  // The `ref` is what `useMediaBlob` observes for visibility. It is `eager`
  // here so nothing depends on it being on-screen, but it must stay mounted
  // across every branch — including inside the lightbox, whose portal moves it
  // out of the timeline.
  if (framed) {
    return (
      <Lightbox
        label={label}
        caption={
          media.caption === null ? null : (
            <MediaCaption
              accountId={accountId}
              caption={media.caption}
              content={content}
            />
          )
        }
        onClose={onClose}
      >
        <div ref={ref} class="lightbox-media">
          {body}
        </div>
      </Lightbox>
    )
  }
  return (
    <div ref={ref} class="media-preview">
      {body}
    </div>
  )
}

/** Reserve the video's shape before metadata loads, capped like a thumbnail —
 *  the timeline is not re-anchored after mount, so a player that grew on load
 *  would shove scrolled-back content around (same reasoning as `MediaImage`). */
function videoStyle(media: ParsedMedia): Record<string, string> {
  if (media.w === undefined || media.h === undefined) {
    return { width: `${VIDEO_MAX}px`, maxWidth: '100%' }
  }
  const scale = Math.min(1, VIDEO_MAX / media.w, VIDEO_MAX / media.h)
  return {
    aspectRatio: `${media.w} / ${media.h}`,
    width: `${Math.round(media.w * scale)}px`,
    maxWidth: '100%',
  }
}

/**
 * The decoded contents of a text attachment, truncated at the kind's ceiling
 * and syntax-highlighted when we can tell what language it is (ADR 0073).
 *
 * Both branches are safe by different means: the plain one is a Preact text
 * child, escaped by the renderer; the highlighted one is `highlight.js` output
 * (which escapes its input) passed through DOMPurify.
 */
function TextPreview({
  accountId,
  mxcUrl,
  filename,
}: {
  accountId: string
  mxcUrl: string
  filename: string
}) {
  const { media: service } = useServices()
  const [text, setText] = useState<string | null>(null)
  const [html, setHtml] = useState<string | null>(null)
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    let cancelled = false
    void service
      .fetchText(accountId, mxcUrl, previewSizeCeiling('text'))
      .then(async (result) => {
        if (cancelled) {
          return
        }
        if (!result.ok) {
          setFailed(true)
          return
        }
        // Show the text first, then upgrade it. The highlighter is a lazy
        // chunk, so awaiting it before painting would hold a readable preview
        // behind a network round trip.
        setText(result.text)
        const language =
          languageForFilename(filename) ?? languageFromShebang(result.text)
        if (language === null) {
          return
        }
        const highlighted = await highlightCode(result.text, language)
        if (!cancelled && highlighted !== null) {
          setHtml(highlighted)
        }
      })
    return () => {
      cancelled = true
    }
  }, [service, accountId, mxcUrl, filename])

  if (failed) {
    return (
      <div class="media-preview">
        <p class="local-echo-status error">Could not load text</p>
      </div>
    )
  }
  if (text === null) {
    return (
      <div class="media-preview">
        <div class="media-skeleton" aria-hidden="true" />
      </div>
    )
  }
  return (
    <div class="media-preview">
      <pre class="media-text">
        {html === null ? (
          <code>{text}</code>
        ) : (
          <code class="hljs" dangerouslySetInnerHTML={{ __html: html }} />
        )}
      </pre>
    </div>
  )
}
