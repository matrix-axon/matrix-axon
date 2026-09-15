import { useEffect, useRef, useState } from 'preact/hooks'

const MESSAGE_GESTURE_FEEDBACK_MS = 1600

/** One short-lived, surface-local gesture status at a time. */
export function useGestureFeedback() {
  const timer = useRef<number | null>(null)
  const [feedback, setFeedback] = useState<{
    eventId: string
    message: string
  } | null>(null)

  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current)
    },
    [],
  )

  const show = (eventId: string, message: string) => {
    setFeedback({ eventId, message })
    if (timer.current !== null) window.clearTimeout(timer.current)
    timer.current = window.setTimeout(() => {
      setFeedback(null)
      timer.current = null
    }, MESSAGE_GESTURE_FEEDBACK_MS)
  }

  return { feedback, show }
}
