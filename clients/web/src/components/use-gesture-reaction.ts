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
  const activeRequests = useRef(new Map<string, number>())
  const nextRequest = useRef(0)
  const burstTimer = useRef<number | null>(null)
  const burstEventId = useRef<string | null>(null)
  const [presentation, setPresentation] =
    useState<GestureReactionPresentation | null>(null)
  const [burst, setBurst] = useState<{
    eventId: string
    emoji: string
  } | null>(null)

  useEffect(
    () => () => {
      if (burstTimer.current !== null) window.clearTimeout(burstTimer.current)
      activeRequests.current.clear()
      burstEventId.current = null
    },
    [],
  )

  const run = (event: TimelineEvent, emoji: string) => {
    // The event remains unchanged until the authoritative refresh lands. Do
    // not issue a second toggle against that event's stale snapshot in the
    // meantime. Other gallery events have independent snapshots and requests.
    if (activeRequests.current.has(event.event_id)) return
    const request = ++nextRequest.current
    const removing = event.reactions?.[emoji]?.me === true
    activeRequests.current.set(event.event_id, request)
    setPresentation({ eventId: event.event_id, emoji, removing })

    if (burstTimer.current !== null) window.clearTimeout(burstTimer.current)
    if (removing) {
      burstEventId.current = null
      setBurst(null)
    } else {
      burstEventId.current = event.event_id
      setBurst({ eventId: event.event_id, emoji })
      burstTimer.current = window.setTimeout(() => {
        if (burstEventId.current === event.event_id) {
          burstEventId.current = null
          setBurst(null)
        }
        burstTimer.current = null
      }, MESSAGE_REACTION_BURST_MS)
    }

    void timeline.toggleReaction(event, emoji).then((ok) => {
      if (activeRequests.current.get(event.event_id) !== request) return
      activeRequests.current.delete(event.event_id)
      setPresentation((current) =>
        current?.eventId === event.event_id ? null : current,
      )
      if (ok) {
        onMutation?.()
        return
      }
      if (
        burstEventId.current === event.event_id &&
        burstTimer.current !== null
      ) {
        window.clearTimeout(burstTimer.current)
        burstTimer.current = null
        burstEventId.current = null
        setBurst(null)
      }
      onFeedback(event.event_id, 'Reaction could not be updated')
    })
  }

  return { presentation, burst, run }
}
