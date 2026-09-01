import { useEffect, useState } from 'preact/hooks'
import type { ComposerAutocompleteOption } from './Composer'
import {
  currentEmojiEntries,
  emojiShortcodeSuggestions,
  loadEmojiEntries,
  type EmojiEntry,
} from '../emoji'
import { useAttachments, type AttachmentBatch } from '../media/use-attachments'
import type { AttachmentStaging } from '../media/attachment-staging'
import { useFileDrop } from '../media/use-file-drop'
import {
  formatMessageBody,
  roomReferenceSuggestions,
  userMentionSuggestions,
  type FormattedMessageParts,
} from '../mentions'
import type { Platform } from '../platform'
import type { MembersStore } from '../stores/members'
import type { RoomDto } from '../stores/room-list'
import type { EventDto, TimelineStore } from '../stores/timeline'

export type ComposerAction =
  | { kind: 'reply'; event: EventDto }
  | { kind: 'edit'; event: EventDto; draft?: string }

export interface MessageComposerOptions {
  /** The store the composer sends into (room- or thread-scoped). */
  timeline: TimelineStore
  accountId: string
  members: MembersStore
  rooms: readonly RoomDto[]
  roomTitles: ReadonlyMap<string, string>
  ownUserId: string | null
  /**
   * Identity of the staged attachment's surface. Must include everything
   * that distinguishes where a send lands — account, room, and thread root
   * where applicable — so a staged file never survives into a surface that
   * would send it from the wrong place.
   */
  attachmentScope: string
  /** Called after a message, edit, or media batch successfully mutates room state. */
  onMutation?: () => void
  /**
   * Where staged files live (issue #89). From the service graph, not this
   * hook: the surfaces that own a composer unmount on a route change, and the
   * files have to outlive that.
   */
  staging: AttachmentStaging
  /**
   * The shell's window-level drag-drop channel, where there is one
   * (`Platform.onNativeFileDrop`). Passed in rather than read from the service
   * graph so this hook keeps taking everything it needs explicitly.
   */
  nativeDrops?: Platform['onNativeFileDrop']
}

/**
 * The composer plumbing RoomPage and ThreadPanel share (reply/edit mode, the
 * banner, attachment staging, mention/room/emoji completions, and the submit
 * pipeline with its WCR-10 edit-draft restore). One implementation so a fix
 * in the send path — the class of bug that loses user text — cannot land in
 * one panel and miss the other. Scope-specific concerns (slash commands,
 * draft persistence, focus, the Composer `key`) stay with the caller.
 */
export function useMessageComposer(options: MessageComposerOptions): {
  action: ComposerAction | null
  setAction: (action: ComposerAction | null) => void
  banner: { label: string; excerpt: string; onCancel: () => void } | undefined
  /** False in edit mode: an edit replaces text, not media. */
  attachable: boolean
  attachments: AttachmentBatch
  stage: (files: FileList | readonly File[]) => void
  removeAttachment: (id: string) => void
  clearAttachment: () => void
  dragging: boolean
  dropHandlers: ReturnType<typeof useFileDrop>['handlers']
  /** Why a drop staged nothing, if it did not. */
  dropProblem: string | null
  emojiEntries: readonly EmojiEntry[]
  formatComposerBody: (body: string) => Promise<FormattedMessageParts>
  mentionCompletions: (query: string) => ComposerAutocompleteOption[]
  roomReferenceCompletions: (query: string) => ComposerAutocompleteOption[]
  emojiCompletions: (query: string) => ComposerAutocompleteOption[]
  /** The Composer `onSubmit`: media, edit, reply, or plain send. */
  submitMessage: (body: string) => Promise<boolean>
} {
  const {
    timeline,
    accountId,
    members,
    rooms,
    roomTitles,
    ownUserId,
    onMutation,
  } = options
  const [action, setAction] = useState<ComposerAction | null>(null)
  const [emojiEntries, setEmojiEntries] = useState(() => currentEmojiEntries())
  const attachable = action?.kind !== 'edit'
  const {
    batch: attachments,
    stage,
    remove: removeAttachment,
    clear: clearAttachment,
  } = useAttachments(options.attachmentScope, options.staging)
  const {
    dragging,
    problem: dropProblem,
    handlers: dropHandlers,
  } = useFileDrop(stage, options.nativeDrops)

  useEffect(() => {
    let cancelled = false
    void loadEmojiEntries().then((entries) => {
      if (!cancelled) {
        setEmojiEntries(entries)
      }
    })
    return () => {
      cancelled = true
    }
  }, [])

  const banner =
    action === null
      ? undefined
      : {
          label: action.kind === 'reply' ? 'Replying to' : 'Editing',
          excerpt: `${action.event.sender}: ${(action.event.body ?? '').slice(0, 80)}`,
          onCancel: () => setAction(null),
        }

  const formatComposerBody = async (body: string) =>
    formatMessageBody(body, {
      accountId,
      members: [...members.members.value.values()],
      rooms,
      roomTitles,
      emojiEntries: await loadEmojiEntries(),
    })

  const mentionCompletions = (query: string): ComposerAutocompleteOption[] =>
    userMentionSuggestions([...members.members.value.values()], query)

  const roomReferenceCompletions = (
    query: string,
  ): ComposerAutocompleteOption[] =>
    roomReferenceSuggestions(rooms, roomTitles, query)

  const emojiCompletions = (query: string): ComposerAutocompleteOption[] =>
    emojiShortcodeSuggestions(emojiEntries, query)

  const submitMessage = async (body: string): Promise<boolean> => {
    // Dismiss the reply/edit mode immediately — don't hold the banner hostage
    // to the network round trip; the timeline renders the pending/failed
    // outcome.
    const current = action
    setAction(null)
    const senderId = ownUserId ?? undefined
    const replyTo =
      current?.kind === 'reply' ? current.event.event_id : undefined
    if (attachments.items.length > 0) {
      // Unstage *before* the caption render: `formatComposerBody` can await a
      // cold emoji-data fetch, and a second Enter during that window used to
      // pass the composer's has-attachment guard and send the files twice.
      const staged = attachments.items.map((item) => item.file)
      clearAttachment()
      // The composer's text captions the *first* image; with none, the server
      // uses each filename as the body (ADR 0065, ADR 0081).
      const caption =
        body === '' ? undefined : (await formatComposerBody(body)).body
      const ok = await timeline.sendMediaBatch(staged, {
        caption,
        replyTo,
        senderId,
      })
      if (ok) onMutation?.()
      return ok
    }
    const formatted = await formatComposerBody(body)
    if (current?.kind === 'edit') {
      const ok = await timeline.edit(current.event.event_id, formatted.body, {
        formattedBody: formatted.formatted_body ?? null,
      })
      if (!ok) {
        // A failed edit has no retryable echo (sends do), so re-open edit
        // mode carrying the text the user typed — the error banner alone
        // would silently discard it (WCR-10).
        setAction({ ...current, draft: body })
      }
      if (ok) onMutation?.()
      return ok
    }
    const ok = await timeline.send(formatted.body, {
      replyTo,
      senderId,
      formattedBody: formatted.formatted_body ?? null,
    })
    if (ok) onMutation?.()
    return ok
  }

  return {
    action,
    setAction,
    banner,
    attachable,
    attachments,
    stage,
    removeAttachment,
    clearAttachment,
    dragging,
    dropProblem,
    dropHandlers,
    emojiEntries,
    formatComposerBody,
    mentionCompletions,
    roomReferenceCompletions,
    emojiCompletions,
    submitMessage,
  }
}
