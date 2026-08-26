import { useMemo } from 'preact/hooks'
import { markdownToHtmlIfFormatted } from '../markdown/markdown'
import { FormattedBody } from './FormattedBody'

/**
 * Caption text rendered through `FormattedBody`.
 *
 * Image sends go through `send-media`, which has no `formatted_body` field, so
 * axon-sent captions land as markdown source in `body`. When the event already
 * carries Matrix HTML (other clients, or an edited caption), that wins;
 * otherwise we convert the caption the same way a message send would.
 */
export function MediaCaption({
  accountId,
  caption,
  content,
}: {
  accountId: string
  caption: string
  content?: unknown
}) {
  const formatted = useMemo(
    () => captionRenderContent(caption, content),
    [caption, content],
  )
  return (
    <FormattedBody accountId={accountId} body={caption} content={formatted} />
  )
}

function captionRenderContent(caption: string, eventContent: unknown): unknown {
  const c = eventContent as
    { format?: unknown; formatted_body?: unknown } | null | undefined
  if (
    c?.format === 'org.matrix.custom.html' &&
    typeof c.formatted_body === 'string'
  ) {
    return eventContent
  }
  const html = markdownToHtmlIfFormatted(caption)
  if (html === null) {
    return {}
  }
  return {
    format: 'org.matrix.custom.html',
    formatted_body: html,
  }
}
