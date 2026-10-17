import { useMemo } from 'preact/hooks'
import { parseLinkPreviews, type LinkPreview } from '../link-previews'

function PreviewCard({ preview }: { preview: LinkPreview }) {
  return (
    <a
      class="link-preview-card"
      href={preview.url}
      target="_blank"
      rel="noopener noreferrer"
    >
      <span class="link-preview-host">{preview.hostname}</span>
      {preview.siteName !== null && (
        <span class="link-preview-site">{preview.siteName}</span>
      )}
      {preview.title !== null && <strong>{preview.title}</strong>}
      {preview.description !== null && (
        <span class="link-preview-description">{preview.description}</span>
      )}
    </a>
  )
}

export function LinkPreviews({
  content,
  body,
}: {
  content: unknown
  body: string | null | undefined
}) {
  const previews = useMemo(
    () => parseLinkPreviews(content, body),
    [content, body],
  )
  if (previews.length === 0) return null
  return (
    <div class="link-previews" aria-label="Link previews">
      {previews.slice(0, 2).map((preview) => (
        <PreviewCard key={preview.url} preview={preview} />
      ))}
      {previews.length > 2 && (
        <details key={previews.map((preview) => preview.url).join('\0')}>
          <summary>Show {previews.length - 2} more previews</summary>
          {previews.slice(2).map((preview) => (
            <PreviewCard key={preview.url} preview={preview} />
          ))}
        </details>
      )}
    </div>
  )
}
