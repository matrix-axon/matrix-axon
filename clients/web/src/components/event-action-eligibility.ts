import type { EventDto, TimelineEvent } from '../stores/timeline'

export function isStateEvent(event: EventDto): boolean {
  return event.state_key !== null && event.state_key !== undefined
}

/** Confirmed, visible events that can use the normal message actions. */
export function isMessageActionable(event: TimelineEvent): boolean {
  return (
    !isStateEvent(event) &&
    !event.redacted &&
    event.localEcho?.status !== 'pending' &&
    event.localEcho?.status !== 'failed'
  )
}

export function isEditable(
  event: TimelineEvent,
  ownUserId: string | null,
): boolean {
  return (
    ownUserId !== null &&
    event.sender === ownUserId &&
    isMessageActionable(event) &&
    event.type === 'm.room.message'
  )
}

export function isReactable(event: TimelineEvent): boolean {
  return isMessageActionable(event)
}
