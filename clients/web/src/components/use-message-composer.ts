import { useCallback, useEffect, useMemo, useState } from 'preact/hooks'
import type { ApiClient } from '../api/client'
import type {
  ComposerAutocompleteOption,
  ComposerLinkPreviewOptions,
  SubmittedPreviews,
} from './Composer'
import {
  homeserverLinkPreview,
  linkPreviewsEnabled,
  type LinkPreviewCache,
  type PreviewSources,
} from '../composer-link-previews'
import { planLinkPreviews, type PreviewDraft } from '../link-preview-send'
import {
  MEDIA_MSGTYPES,
  messageCards,
  messagePreviewField,
  planEditPreviews,
  type EditLinkPreview,
  type EditPreviewDraft,
  type EditPreviewSeed,
} from '../edit-link-previews'
import { linkIdentity } from '../preview-links'
import type { ServerCapabilities } from '../stores/capabilities'
import type { LinkPreviewSetting } from '../stores/settings'
import {
  inlineEmojiFromEvent,
  type InlineEmojiReference,
} from '../inline-emoji'
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
  | {
      kind: 'edit'
      event: EventDto
      draft?: string
      inlineEmoji?: readonly InlineEmojiReference[]
      /** Edit-mode link previews (ADR 0132 § 8), decided as edit mode opens. */
      previews?: EditPreviewsState
    }

/** What edit mode knows about the message's link previews. */
export interface EditPreviewsState {
  /**
   * The server keeps, removes and adds previews on an edit, as known when
   * edit mode opened.
   */
  supported: boolean
  /**
   * Edit mode opened before the server answered. A yes that comes later
   * upgrades this edit to cards, re-seeded from the message.
   */
  pending?: boolean
  /** Links discarded earlier in this edit, kept across a failed save. */
  discarded?: ReadonlySet<string>
  /** Links whose new cards the server refused, which get none again. */
  refused?: ReadonlySet<string>
  /** Why edit mode reopened, shown with the banner. */
  notice?: string
}

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
  /**
   * Composer link previews (ADR 0132): the setting, this device's fetcher
   * where the platform has one, the API for the homeserver, and the shared
   * result cache. Absent for none.
   */
  linkPreviews?: {
    setting: LinkPreviewSetting
    device?: Platform['linkPreview']
    api: ApiClient
    cache: LinkPreviewCache
    /** Whether the server keeps previews on an edit (ADR 0132 § 8). */
    capabilities?: ServerCapabilities
  }
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
  banner:
    | { label: string; excerpt: string; note?: string; onCancel: () => void }
    | undefined
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
  editInlineEmoji: readonly InlineEmojiReference[]
  formatComposerBody: (
    body: string,
    inlineEmoji?: readonly InlineEmojiReference[],
  ) => Promise<FormattedMessageParts>
  mentionCompletions: (query: string) => ComposerAutocompleteOption[]
  roomReferenceCompletions: (query: string) => ComposerAutocompleteOption[]
  emojiCompletions: (query: string) => ComposerAutocompleteOption[]
  /** The Composer `onSubmit`: media, edit, reply, or plain send. */
  submitMessage: (
    body: string,
    inlineEmoji?: readonly InlineEmojiReference[],
    previews?: SubmittedPreviews,
  ) => Promise<boolean>
  /** The Composer's `linkPreviews`; absent in edit mode or without them. */
  composerLinkPreviews: ComposerLinkPreviewOptions | undefined
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
  const [action, setActionState] = useState<ComposerAction | null>(null)
  const capabilities = options.linkPreviews?.capabilities
  // Edit mode reads the server's answer as it opens, and asks again when the
  // answer is not known yet (ADR 0132 § 8).
  const setAction = useCallback(
    (next: ComposerAction | null) => {
      if (next?.kind === 'edit' && next.previews === undefined) {
        const known = capabilities?.editUrlPreviews.peek() ?? null
        if (known === null) capabilities?.refresh()
        next = {
          ...next,
          previews: {
            supported: known === true,
            ...(known === null && { pending: true }),
          },
        }
      }
      setActionState(next)
    },
    [capabilities],
  )
  // A yes that comes while such an edit is open upgrades it: a new action,
  // so the composer re-seeds its cards from the message, keeping the text.
  const editAnswer = capabilities?.editUrlPreviews.value ?? null
  useEffect(() => {
    if (
      action?.kind !== 'edit' ||
      action.previews?.pending !== true ||
      editAnswer !== true
    )
      return
    setActionState({
      ...action,
      previews: { ...action.previews, supported: true, pending: undefined },
    })
  }, [action, editAnswer])
  const editInlineEmoji = useMemo(
    () =>
      action?.kind === 'edit'
        ? (action.inlineEmoji ??
          (action.draft !== undefined
            ? []
            : inlineEmojiFromEvent(
                action.event.body ?? '',
                action.event.content,
              )))
        : [],
    [action],
  )
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
  } = useFileDrop(stage, {
    nativeDrops: options.nativeDrops,
    scope: options.attachmentScope,
  })

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

  const editCards = useMemo(
    () =>
      action?.kind === 'edit'
        ? messageCards(action.event.content, action.event.body)
        : [],
    [action],
  )
  const editNote =
    action?.kind !== 'edit'
      ? undefined
      : (action.previews?.notice ??
        (action.previews?.supported !== true && editCards.length > 0
          ? 'Saving this edit may remove its link previews.'
          : undefined))
  const banner =
    action === null
      ? undefined
      : {
          label: action.kind === 'reply' ? 'Replying to' : 'Editing',
          excerpt: `${action.event.sender}: ${(action.event.body ?? '').slice(0, 80)}`,
          ...(editNote !== undefined && { note: editNote }),
          onCancel: () => setAction(null),
        }

  const formatComposerBody = async (
    body: string,
    inlineEmoji?: readonly InlineEmojiReference[],
  ) =>
    formatMessageBody(body, {
      accountId,
      members: [...members.members.value.values()],
      rooms,
      roomTitles,
      emojiEntries: await loadEmojiEntries(),
      inlineEmoji,
    })

  const mentionCompletions = (query: string): ComposerAutocompleteOption[] =>
    userMentionSuggestions([...members.members.value.values()], query)

  const roomReferenceCompletions = (
    query: string,
  ): ComposerAutocompleteOption[] =>
    roomReferenceSuggestions(rooms, roomTitles, query)

  const emojiCompletions = (query: string): ComposerAutocompleteOption[] =>
    emojiShortcodeSuggestions(emojiEntries, query)

  /** The `url_previews` decision for this exact request (ADR 0132 § 6). The
   *  thread root comes from the store, which is how a thread send gets it. */
  const planPreviews = (
    formatted: FormattedMessageParts,
    replyTo: string | undefined,
    draft: PreviewDraft | undefined,
    allowUnmarked = false,
  ) =>
    planLinkPreviews(
      {
        body: formatted.body,
        ...(formatted.formatted_body != null && {
          format: 'org.matrix.custom.html' as const,
          formatted_body: formatted.formatted_body,
        }),
        reply_to: replyTo ?? null,
        thread_root: timeline.threadRoot ?? null,
      },
      draft,
      allowUnmarked,
    )

  const previewApi = options.linkPreviews?.api
  const homeserver = useMemo(
    () =>
      previewApi === undefined
        ? undefined
        : homeserverLinkPreview(previewApi, accountId),
    [previewApi, accountId],
  )
  const sources: PreviewSources | undefined =
    options.linkPreviews === undefined
      ? undefined
      : {
          setting: options.linkPreviews.setting,
          device: options.linkPreviews.device,
          homeserver,
        }
  /** The edit's request as `planEditPreviews` measures it. */
  const editRequest = (event: EventDto, formatted: FormattedMessageParts) => ({
    eventId: event.event_id,
    body: formatted.body,
    formattedBody: formatted.formatted_body ?? null,
    content: event.content,
  })
  const planEdit = (
    event: EventDto,
    formatted: FormattedMessageParts,
    draft: EditPreviewDraft,
    allowUnmarked = false,
  ) =>
    planEditPreviews(editRequest(event, formatted), draft, {
      originalText: event.body ?? '',
      previewsOn: sources !== undefined && linkPreviewsEnabled(sources),
      allowUnmarked,
    })
  // The message an edit starts from, one object per opening of edit mode:
  // the composer re-seeds its cards whenever this changes, including a
  // reopen after a failed save whose text is unchanged.
  const editSeed: EditPreviewSeed | undefined = useMemo(
    () =>
      action?.kind === 'edit'
        ? {
            originalText: action.event.body ?? '',
            cards: editCards,
            addNew: !MEDIA_MSGTYPES.has(action.event.content?.msgtype),
            discarded: action.previews?.discarded,
            refused: action.previews?.refused,
          }
        : undefined,
    [action, editCards],
  )
  // An edit against a server that keeps previews starts from the message's
  // cards, under any setting, since removing one fetches nothing (§ 8).
  const editLinkPreviews: ComposerLinkPreviewOptions | undefined =
    action?.kind !== 'edit' ||
    action.previews?.supported !== true ||
    editSeed === undefined ||
    sources === undefined ||
    options.linkPreviews === undefined
      ? undefined
      : {
          sources,
          cache: options.linkPreviews.cache,
          edit: {
            seed: editSeed,
            accountId,
            hasField: messagePreviewField(action.event.content).hasField,
          },
          markerFits: async (body, inlineEmoji, draft) => {
            const formatted = await formatComposerBody(body, inlineEmoji)
            return (
              planEdit(action.event, formatted, draft).kind !==
              'too-long-for-marker'
            )
          },
        }
  // Off, an edit, or no source for the setting: the composer authors no
  // previews, so a send leaves the field out exactly as before (§ 6 rule 1).
  const composerLinkPreviews: ComposerLinkPreviewOptions | undefined =
    action?.kind === 'edit'
      ? editLinkPreviews
      : sources === undefined ||
          options.linkPreviews === undefined ||
          !linkPreviewsEnabled(sources)
        ? undefined
        : {
            sources,
            cache: options.linkPreviews.cache,
            markerFits: async (body, inlineEmoji, draft) => {
              const replyTo =
                action?.kind === 'reply' ? action.event.event_id : undefined
              const formatted = await formatComposerBody(body, inlineEmoji)
              return (
                planPreviews(formatted, replyTo, draft).kind !==
                'too-long-for-marker'
              )
            },
          }

  const submitMessage = async (
    body: string,
    inlineEmoji?: readonly InlineEmojiReference[],
    previews?: SubmittedPreviews,
  ): Promise<boolean> => {
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
        body === '' ? undefined : await formatComposerBody(body, inlineEmoji)
      const ok = await timeline.sendMediaBatch(staged, {
        caption: caption?.body,
        formattedBody: caption?.formatted_body,
        replyTo,
        senderId,
      })
      if (ok) onMutation?.()
      return ok
    }
    const formatted = await formatComposerBody(body, inlineEmoji)
    if (current?.kind === 'edit') {
      // Previews only against a server that keeps them; otherwise the field
      // is left out, as before ADR 0132 § 8.
      const draft = previews?.draft
      const plan =
        current.previews?.supported === true && draft !== undefined
          ? planEdit(current.event, formatted, draft, previews?.allowUnmarked)
          : ({ kind: 'omit' } as const)
      const linkPreviews: EditLinkPreview[] | undefined =
        plan.kind === 'send'
          ? plan.previews
          : // Asked before saving, so this is a race with the formatter.
            // Keep the marker: never drop it, or expose a link, unasked.
            plan.kind === 'too-long-for-marker'
            ? []
            : undefined
      let failure: { kind: 'rejected' | 'changed'; event?: EventDto } | null =
        null
      const ok = await timeline.edit(current.event.event_id, formatted.body, {
        formattedBody: formatted.formatted_body ?? null,
        ...(linkPreviews !== undefined && {
          linkPreviews,
          onPreviewFailure: (kind, event) => {
            failure = { kind, event }
          },
        }),
      })
      if (!ok) {
        // A failed edit has no retryable echo (sends do), so re-open edit
        // mode carrying the text the user typed — the error banner alone
        // would silently discard it (WCR-10) — and, with previews, the
        // links the user discarded.
        const reopened: EditPreviewsState | undefined =
          current.previews === undefined
            ? undefined
            : { ...current.previews, discarded: draft?.discarded }
        const reason = failure as {
          kind: 'rejected' | 'changed'
          event?: EventDto
        } | null
        // Only a rejection refuses the new cards; any other failure reopens
        // with them, to be sent again.
        const refused = new Set(current.previews?.refused)
        if (reason?.kind === 'rejected')
          for (const card of draft?.cards ?? []) {
            const identity = linkIdentity(card.matchedUrl)
            if (identity !== null) refused.add(identity)
          }
        setAction({
          ...current,
          ...(reason?.kind === 'changed' &&
            reason.event !== undefined && { event: reason.event }),
          draft: body,
          inlineEmoji: inlineEmoji ?? [],
          previews:
            reopened === undefined
              ? undefined
              : reason?.kind === 'rejected'
                ? {
                    ...reopened,
                    refused,
                    notice:
                      'The server refused the new link previews. Save again to keep the rest.',
                  }
                : reason?.kind === 'changed'
                  ? {
                      ...reopened,
                      notice:
                        'This message’s link previews changed on another device. Check them and save again.',
                    }
                  : reopened,
        })
      }
      if (ok) onMutation?.()
      return ok
    }
    // Previews are authored only for a plain send or a reply: an edit and a
    // media batch have returned above, and leave the field out (§ 6 rule 1).
    const plan = planPreviews(
      formatted,
      replyTo,
      previews?.draft,
      previews?.allowUnmarked,
    )
    const ok = await timeline.send(formatted.body, {
      replyTo,
      senderId,
      formattedBody: formatted.formatted_body ?? null,
      ...(plan.kind === 'send' && { linkPreviews: plan.previews }),
      // The composer asked before sending, so this is a race with the
      // formatter. Keep the marker: never expose a discarded link unasked.
      ...(plan.kind === 'too-long-for-marker' && { linkPreviews: [] }),
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
    editInlineEmoji,
    formatComposerBody,
    mentionCompletions,
    roomReferenceCompletions,
    emojiCompletions,
    submitMessage,
    composerLinkPreviews,
  }
}
