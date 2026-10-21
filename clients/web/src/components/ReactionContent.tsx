import { useEffect, useState } from 'preact/hooks'
import { mxcToPath } from '../media/parse-media'
import { useMediaBlob } from '../media/use-media-blob'

/** Match the server's structural and byte-length limits for reaction keys. */
export function customReactionMxc(key: string): string | null {
  if (key.length > 2048 || new TextEncoder().encode(key).length > 2048) {
    return null
  }
  const path = mxcToPath(key)
  return path !== null && !path.mediaId.includes('/') ? key : null
}

export function reactionLabel(key: string): string {
  return customReactionMxc(key) === null ? key : 'Custom reaction'
}

/** A keyed child keeps async state scoped to both the account and object. */
export function ReactionContent({
  accountId,
  reaction,
}: {
  accountId: string
  reaction: string
}) {
  const mxc = customReactionMxc(reaction)
  return mxc === null ? (
    <>{reaction}</>
  ) : (
    <ReactionImage
      key={`${accountId}\0${mxc}`}
      accountId={accountId}
      mxc={mxc}
    />
  )
}

function ReactionImage({ accountId, mxc }: { accountId: string; mxc: string }) {
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
          alt=""
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
