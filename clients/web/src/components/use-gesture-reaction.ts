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
  const burstTimers = useRef(new Map<string, number>())
  const [presentations, setPresentations] = useState<
    ReadonlyMap<string, GestureReactionPresentation>
  >(new Map())
  const [bursts, setBursts] = useState<ReadonlyMap<string, string>>(new Map())

  useEffect(
    () => () => {
      for (const timer of burstTimers.current.values()) {
        window.clearTimeout(timer)
      }
      burstTimers.current.clear()
      activeRequests.current.clear()
    },
    [],
  )

  const clearBurst = (eventId: string) => {
    const timer = burstTimers.current.get(eventId)
    if (timer !== undefined) {
      window.clearTimeout(timer)
      burstTimers.current.delete(eventId)
    }
    setBursts((current) => {
      if (!current.has(eventId)) return current
      const next = new Map(current)
      next.delete(eventId)
      return next
    })
  }

  const run = (event: TimelineEvent, emoji: string) => {
    // The event remains unchanged until the authoritative refresh lands. Do
    // not issue a second toggle against that event's stale snapshot in the
    // meantime. Other gallery events have independent snapshots and requests.
    if (activeRequests.current.has(event.event_id)) return
    const request = ++nextRequest.current
    const removing = event.reactions?.[emoji]?.me === true
    activeRequests.current.set(event.event_id, request)
    setPresentations((current) =>
      new Map(current).set(event.event_id, {
        eventId: event.event_id,
        emoji,
        removing,
      }),
    )

    clearBurst(event.event_id)
    // A removal has only the pending chip, not an adding burst.
    if (!removing) {
      setBursts((current) => new Map(current).set(event.event_id, emoji))
      const timer = window.setTimeout(() => {
        if (burstTimers.current.get(event.event_id) === timer) {
          clearBurst(event.event_id)
        }
      }, MESSAGE_REACTION_BURST_MS)
      burstTimers.current.set(event.event_id, timer)
    }

    void timeline.toggleReaction(event, emoji).then((ok) => {
      if (activeRequests.current.get(event.event_id) !== request) return
      activeRequests.current.delete(event.event_id)
      setPresentations((current) => {
        if (!current.has(event.event_id)) return current
        const next = new Map(current)
        next.delete(event.event_id)
        return next
      })
      if (ok) {
        onMutation?.()
        return
      }
      clearBurst(event.event_id)
      onFeedback(event.event_id, 'Reaction could not be updated')
    })
  }

  return { presentations, bursts, run }
}
