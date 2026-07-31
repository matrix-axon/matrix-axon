export type EventActionIconName =
  | 'reply'
  | 'thread'
  | 'react'
  | 'edit'
  | 'delete'
  | 'confirm'
  | 'cancel'
  | 'inspect'

/** Shared iconography for message actions in rows and the media viewer. */
export function EventActionIcon({ name }: { name: EventActionIconName }) {
  return (
    <svg
      class="event-action-icon"
      viewBox="0 0 24 24"
      aria-hidden="true"
      focusable="false"
    >
      {eventActionIconPath(name)}
    </svg>
  )
}

function eventActionIconPath(name: EventActionIconName) {
  switch (name) {
    case 'reply':
      return (
        <path
          d="M9 10 4 15l5 5m-5-5h9a7 7 0 0 0 7-7V5"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
          stroke-width="2"
        />
      )
    case 'thread':
      return (
        <>
          <path
            d="M21 12a8 8 0 0 1-8 8H7l-4 3v-7a8 8 0 1 1 18-4Z"
            fill="none"
            stroke="currentColor"
            stroke-linecap="round"
            stroke-linejoin="round"
            stroke-width="2"
          />
          <path
            d="M8 11h8M8 15h5"
            fill="none"
            stroke="currentColor"
            stroke-linecap="round"
            stroke-width="2"
          />
        </>
      )
    case 'react':
      return (
        <>
          <circle
            cx="12"
            cy="12"
            r="8"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
          />
          <path
            d="M9 10h.01M15 10h.01M9 14a4 4 0 0 0 6 0M19 5v4M17 7h4"
            fill="none"
            stroke="currentColor"
            stroke-linecap="round"
            stroke-linejoin="round"
            stroke-width="2"
          />
        </>
      )
    case 'edit':
      return (
        <path
          d="m4 20 4.5-1 10-10a2.1 2.1 0 0 0-3-3l-10 10L4 20Zm12-13 3 3"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
          stroke-width="2"
        />
      )
    case 'delete':
      return (
        <path
          d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
          stroke-width="2"
        />
      )
    case 'confirm':
      return (
        <path
          d="m5 12 4 4L19 6"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
          stroke-width="2"
        />
      )
    case 'cancel':
      return (
        <path
          d="M6 6l12 12M18 6 6 18"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
          stroke-width="2"
        />
      )
    case 'inspect':
      return (
        <path
          d="m8 9-4 3 4 3m8-6 4 3-4 3m-2-9-4 12"
          fill="none"
          stroke="currentColor"
          stroke-linecap="round"
          stroke-linejoin="round"
          stroke-width="2"
        />
      )
  }
}
