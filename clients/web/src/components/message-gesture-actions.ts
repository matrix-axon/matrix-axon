import type { MessageGestureAction } from '../stores/message-gestures'
import type { TimelineEvent } from '../stores/timeline'
import { isEditable } from './event-action-eligibility'

export function messageGestureActionLabel(
  action: MessageGestureAction,
): string {
  switch (action) {
    case 'reply':
      return 'Reply'
    case 'thread':
      return 'Thread'
    case 'react':
      return 'React'
    case 'edit':
      return 'Edit'
    case 'delete':
      return 'Delete'
  }
}

/** Apply one configured action with eligibility shared by every surface. */
export function runAvailableMessageGestureAction(
  action: MessageGestureAction,
  {
    event,
    ownUserId,
    canOpenThread,
    reactionEmoji,
    onReply,
    onOpenThread,
    onReact,
    onEdit,
    onDelete,
    onUnavailable,
  }: {
    event: TimelineEvent
    ownUserId: string | null
    canOpenThread: boolean
    reactionEmoji: string
    onReply: () => void
    onOpenThread: () => void
    onReact: (emoji: string) => void
    onEdit: () => void
    onDelete: () => void
    onUnavailable: (message: string) => void
  },
): void {
  switch (action) {
    case 'reply':
      onReply()
      return
    case 'thread':
      if (!canOpenThread) {
        onUnavailable('Thread is unavailable for this message')
        return
      }
      onOpenThread()
      return
    case 'react':
      onReact(reactionEmoji)
      return
    case 'edit':
      if (!isEditable(event, ownUserId)) {
        onUnavailable('Edit is unavailable for this message')
        return
      }
      onEdit()
      return
    case 'delete':
      if (ownUserId === null || event.sender !== ownUserId) {
        onUnavailable('Delete is unavailable for this message')
        return
      }
      onDelete()
  }
}
