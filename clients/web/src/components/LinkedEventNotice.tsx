/**
 * Why a followed Matrix event link (a `matrix.to` / `matrix:` permalink, a
 * search hit, a reply jump) did not land on its target. Before this the view
 * silently fell back to the newest messages, which reads as "the link did
 * nothing".
 *
 * `missing` is only ever set once the target is verified absent: the server
 * answered 404 for it *and* it is not in the loaded slice. A 5xx, a rate limit
 * or an offline lookup says nothing about the event and must not accuse it.
 */
export type LinkedEventProblem = 'missing' | 'deleted' | 'filtered'

export const LINKED_EVENT_MESSAGES: Record<LinkedEventProblem, string> = {
  missing:
    "The linked message isn't available. Axon has no copy of it: it may be older than the history this account can see in the room, or it may not have synced yet.",
  deleted:
    'The linked message was deleted, and deleted messages are hidden by your settings.',
  filtered:
    "The linked event isn't shown in the timeline with your current settings.",
}

export const LINKED_THREAD_ROOT_MISSING =
  "This thread's first message isn't available. Axon has no copy of it, so only the replies it does have are shown."

/**
 * The dismissable banner for a link that could not be followed. Same markup as
 * `ErrorBanner` (an announced `role="alert"`), but driven by plain props: the
 * problem is page-local state, and one of its sources is derived during render
 * rather than written to a signal.
 */
export function LinkedEventNotice({
  message,
  onDismiss,
}: {
  message: string | null
  onDismiss: () => void
}) {
  if (message === null) {
    return null
  }
  return (
    <div class="banner error linked-event-notice" role="alert">
      <span>{message}</span>
      <button type="button" class="ghost" onClick={onDismiss}>
        Dismiss
      </button>
    </div>
  )
}
