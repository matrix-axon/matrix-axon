import { useEffect, useState } from 'preact/hooks'
import { customReactionMxc, reactionLabel } from '../custom-reactions'
export { customReactionMxc, reactionLabel } from '../custom-reactions'
import { useMediaBlob } from '../media/use-media-blob'

/** A keyed child keeps async state scoped to both the account and object. */
export function ReactionContent({
  accountId,
  reaction,
  shortcode,
}: {
  accountId: string
  reaction: string
  shortcode?: unknown
}) {
  const mxc = customReactionMxc(reaction)
  return mxc === null ? (
    <>{reaction}</>
  ) : (
    <ReactionImage
      key={`${accountId}\0${mxc}`}
      accountId={accountId}
      mxc={mxc}
      label={reactionLabel(reaction, shortcode)}
    />
  )
}

function ReactionImage({
  accountId,
  mxc,
  label,
}: {
  accountId: string
  mxc: string
  label: string
}) {
  const [original, setOriginal] = useState(false)
  const [failedUrl, setFailedUrl] = useState<string | null>(null)
  const { ref, state, invalidate } = useMediaBlob<HTMLSpanElement>(
    accountId,
    mxc,
    {
      thumbnail: original
        ? undefined
        : { width: 32, height: 32, method: 'scale' },
    },
  )
  // Both a failed thumbnail fetch and a decode error try the original once.
  // Keep a failed blob hidden while the hook switches requests after paint.
  useEffect(() => {
    if (state.status === 'error' && !original) {
      setOriginal(true)
    }
  }, [state.status, original])
  const ready = state.status === 'ready' && state.url !== failedUrl
  return (
    <span ref={ref} class="reaction-image" aria-hidden="true">
      {ready ? (
        <img
          src={state.url}
          alt={label}
          width="20"
          height="20"
          decoding="async"
          onError={() => {
            invalidate()
            setFailedUrl(state.url ?? null)
            setOriginal(true)
          }}
        />
      ) : (
        <span
          title={
            state.status === 'error' || failedUrl !== null
              ? 'Reaction image unavailable'
              : 'Loading reaction image'
          }
        >
          ◇
        </span>
      )}
    </span>
  )
}
