import type { ComponentChildren } from 'preact'
import { useEffect, useRef, useState } from 'preact/hooks'
import { copyText } from '../copy-text'

export type CopyStatus = 'idle' | 'copied' | 'failed'

/** Brief Copied / failed feedback around a clipboard write. */
export function useCopyFeedback(idleMs = 1800): {
  status: CopyStatus
  copy: (text: string) => Promise<boolean>
} {
  const [status, setStatus] = useState<CopyStatus>('idle')
  const timer = useRef<number | null>(null)
  useEffect(() => {
    return () => {
      if (timer.current !== null) {
        window.clearTimeout(timer.current)
      }
    }
  }, [])

  const copy = async (text: string): Promise<boolean> => {
    if (timer.current !== null) {
      window.clearTimeout(timer.current)
      timer.current = null
    }
    const ok = await copyText(text)
    setStatus(ok ? 'copied' : 'failed')
    timer.current = window.setTimeout(() => {
      setStatus('idle')
      timer.current = null
    }, idleMs)
    return ok
  }

  return { status, copy }
}

/**
 * A value that copies itself on click or tap. Looks like the surrounding text,
 * not a chrome button — IDs and version strings should be easy to grab.
 */
export function CopyableText({
  text,
  label,
  children,
}: {
  text: string
  /** Used in the accessible name: "Copy {label}". */
  label: string
  children?: ComponentChildren
}) {
  const { status, copy } = useCopyFeedback()
  const title =
    status === 'copied'
      ? 'Copied'
      : status === 'failed'
        ? 'Could not copy'
        : `Copy ${label}`
  return (
    <>
      <button
        type="button"
        class={`copyable-text${status === 'failed' ? ' failed' : ''}`}
        title={title}
        aria-label={`Copy ${label}`}
        onClick={() => void copy(text)}
      >
        {children ?? text}
      </button>
      {status !== 'idle' && (
        <span
          class={`event-copy-status${status === 'failed' ? ' error' : ''}`}
          role="status"
        >
          {status === 'copied' ? 'Copied' : 'Copy failed'}
        </span>
      )}
    </>
  )
}
