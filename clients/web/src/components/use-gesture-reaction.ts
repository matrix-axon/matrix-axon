import { useEffect, useRef, useState } from 'preact/hooks'
import type { TimelineEvent, TimelineStore } from '../stores/timeline'

const MESSAGE_REACTION_BURST_MS = 450

export type GestureReactionPresentation = {
  eventId: string
  emoji: string
  removing: boolean
}

/** Optimistic reaction feedback shared by rows and collapsed gallery tiles. */
export function useGestureReaction({
  timeline,
  onMutation,
  onFeedback,
}: {
  timeline: TimelineStore
  onMutation?: () => void
  onFeedback: (eventId: string, message: string) => void
}) {
  const activeRequest = useRef<number | null>(null)
  const nextRequest = useRef(0)
  const burstTimer = useRef<number | null>(null)
  const [presentation, setPresentation] =
    useState<GestureReactionPresentation | null>(null)
  const [burst, setBurst] = useState<{
    eventId: string
    emoji: string
  } | null>(null)

  useEffect(
    () => () => {
      if (burstTimer.current !== null) window.clearTimeout(burstTimer.current)
      activeRequest.current = null
    },
    [],
  )

  const run = (event: TimelineEvent, emoji: string) => {
    // The event remains unchanged until the authoritative refresh lands. Do
    // not issue a second toggle against that stale snapshot in the meantime.
    if (activeRequest.current !== null) return
    const request = ++nextRequest.current
    const removing = event.reactions?.[emoji]?.me === true
    activeRequest.current = request
    setPresentation({ eventId: event.event_id, emoji, removing })

    if (burstTimer.current !== null) window.clearTimeout(burstTimer.current)
    if (removing) {
      setBurst(null)
    } else {
      setBurst({ eventId: event.event_id, emoji })
      burstTimer.current = window.setTimeout(() => {
        setBurst(null)
        burstTimer.current = null
      }, MESSAGE_REACTION_BURST_MS)
    }

    void timeline.toggleReaction(event, emoji).then((ok) => {
      if (activeRequest.current !== request) return
      activeRequest.current = null
      setPresentation(null)
      if (ok) {
        onMutation?.()
        return
      }
      if (burstTimer.current !== null) {
        window.clearTimeout(burstTimer.current)
        burstTimer.current = null
      }
      setBurst(null)
      onFeedback(event.event_id, 'Reaction could not be updated')
    })
  }

  return { presentation, burst, run }
}
