/** Shared download glyph and touch target for attachment cards and lightboxes. */
export function MediaDownloadButton({
  onClick,
  downloading = false,
  label = 'Download',
  className = '',
}: {
  onClick: () => void
  downloading?: boolean
  label?: string
  className?: string
}) {
  return (
    <button
      type="button"
      class={`ghost media-download-button ${className}`}
      aria-label={label}
      aria-busy={downloading}
      title={label}
      disabled={downloading}
      onClick={onClick}
    >
      <span aria-hidden="true">{downloading ? '…' : '⤓'}</span>
    </button>
  )
}
