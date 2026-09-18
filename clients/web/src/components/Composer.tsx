import type { JSX } from 'preact'
import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks'
import { SINGLE_PANE_QUERY, useMediaQuery } from '../layout'
import { humanSize } from '../media/human-size'
import { hasModifier, isApplePlatform } from '../shortcuts'
import {
  SLASH_COMMAND,
  SLASH_COMMANDS,
  canonicalSlashCommandName,
  type SlashCommandSpec,
} from '../slash-commands'

export interface ComposerAutocompleteOption {
  value: string
  label: string
  description: string
}

type AutocompleteKind =
  'command' | 'room' | 'mention' | 'room-reference' | 'emoji'

function markdownLinkForSelection(
  label: string,
  pasted: string,
): string | null {
  const url = pasted.trim()
  if (url === '' || !isHttpUrl(url)) {
    return null
  }
  return `[${escapeMarkdownLinkLabel(label)}](${escapeMarkdownLinkDestination(url)})`
}

function isHttpUrl(value: string): boolean {
  if (/\s/.test(value)) {
    return false
  }
  try {
    const parsed = new URL(value)
    return parsed.protocol === 'http:' || parsed.protocol === 'https:'
  } catch {
    return false
  }
}

const COMPOSER_MIN_HEIGHT = 38
const COMPOSER_KEYBOARD_RESIZE_STEP = 24
const MOBILE_COMPOSER_MAX_LINES = 4

function cssPixelValue(value: string, fallback: number): number {
  const parsed = Number.parseFloat(value)
  return Number.isFinite(parsed) ? parsed : fallback
}

function escapeMarkdownLinkLabel(value: string): string {
  return value
    .replace(/\\/g, '\\\\')
    .replace(/\[/g, '\\[')
    .replace(/\]/g, '\\]')
}

function escapeMarkdownLinkDestination(value: string): string {
  return value.replace(/([\\)])/g, '\\$1')
}

/**
 * The message composer (ADR 0046, M-W7): a plain textarea with
 * markdown-on-send (conversion happens in the timeline store). Enter sends,
 * Shift+Enter inserts a newline, Escape cancels the active reply/edit.
 *
 * Submitting clears the draft immediately rather than waiting on `onSubmit`'s
 * network round trip — the UI must never lock up while a send is in flight.
 * The store already renders the outcome (a pending/failed row with
 * retry/discard for sends, the top-level error banner for edit/redact), so
 * the composer doesn't need to hold the draft hostage to confirm it first.
 *
 * The parent owns the mode: a `banner` describes an in-progress reply or
 * edit with its cancel action; `initialValue` prefills an edit (parents
 * remount with `key` to reset).
 */
export function Composer({
  placeholder,
  ariaLabel = placeholder,
  status,
  banner,
  initialValue = '',
  onSubmit,
  onCommand,
  roomCompletions,
  mentionCompletions,
  roomReferenceCompletions,
  emojiCompletions,
  onDraftChange,
  onEditLast,
  attachments,
  onAttach,
  autoFocus = false,
  onFocus,
  focusRequest,
  height,
  onHeightChange,
}: {
  placeholder: string
  ariaLabel?: string
  status?: string
  banner?: { label: string; excerpt: string; onCancel: () => void }
  initialValue?: string
  onSubmit(body: string): Promise<boolean>
  /**
   * Intercepts submitted slash commands. Return true when the command was
   * handled and the composer should clear; false leaves the text in place.
   */
  onCommand?(body: string): boolean | Promise<boolean>
  roomCompletions?(query: string): ComposerAutocompleteOption[]
  mentionCompletions?(query: string): ComposerAutocompleteOption[]
  roomReferenceCompletions?(query: string): ComposerAutocompleteOption[]
  emojiCompletions?(query: string): ComposerAutocompleteOption[]
  /**
   * Called on every edit (and with `''` on send) so the parent can persist the
   * draft (M-W6 step 5b). The parent debounces the write; the composer just
   * reports keystrokes.
   */
  onDraftChange?(value: string): void
  /**
   * `ArrowUp` on an empty composer, with no reply/edit already in progress
   * (ADR 0078). The parent decides which message that is, and whether one
   * exists at all — omit to disable, as the thread panel's composer does.
   */
  onEditLast?(): void
  /**
   * The file staged for a media send (ADR 0065), owned by the parent — the same
   * split as `banner`. Rendered as a chip above the field; the composer's text
   * becomes its caption on submit.
   */
  attachments?: {
    items: readonly { id: string; file: File; previewUrl: string | null }[]
    /** Files the caps refused — surfaced, never silently dropped. */
    skipped: number
    skippedReason: 'count' | 'size' | null
    /** The batch's own total, so the strip does not re-add the sizes. */
    totalBytes: number
    onRemove(id: string): void
  }
  /**
   * A file arrived from the picker or a paste. Omit to disable attaching, as
   * the edit-mode composer does — an edit replaces text, not media.
   */
  onAttach?(files: FileList | readonly File[]): void
  /** Focus this composer when it first mounts. */
  autoFocus?: boolean
  onFocus?(): void
  /**
   * A counter the parent bumps to pull focus here (ADR 0078). The value is
   * meaningless; only a change matters.
   */
  focusRequest?: number
  /** Persisted textarea height in CSS pixels; undefined keeps local-only state. */
  height?: number | null
  onHeightChange?(height: number | null): void
}) {
  const [draft, setDraft] = useState(initialValue)
  const [composerHeight, setComposerHeight] = useState<number | null>(null)
  const currentComposerHeight = height === undefined ? composerHeight : height
  const [autocompleteDismissedFor, setAutocompleteDismissedFor] = useState<
    string | null
  >(null)
  const [selectedCommand, setSelectedCommand] = useState(0)
  const [caret, setCaret] = useState(initialValue.length)
  /** The draft value this field is in sync with — the last `initialValue`. */
  const synced = useRef(initialValue)
  const textarea = useRef<HTMLTextAreaElement>(null)
  const fileInput = useRef<HTMLInputElement>(null)
  const singlePane = useMediaQuery(SINGLE_PANE_QUERY)
  /** Only a composer with a command handler treats a leading `/` as a command. */
  const commandsEnabled = onCommand !== undefined
  const commandQuery = commandsEnabled ? slashCommandQuery(draft) : null
  const roomQuery = commandsEnabled ? roomCommandQuery(draft) : null
  const referenceQuery = referenceTokenQuery(draft, caret)
  const reactionEmojiQuery = commandsEnabled
    ? reactCommandEmojiQuery(draft, caret)
    : null
  const emojiQuery =
    reactionEmojiQuery ??
    (referenceQuery?.kind === 'emoji' ? referenceQuery : null)
  const commandMatches =
    commandQuery === null
      ? []
      : SLASH_COMMANDS.filter((command) =>
          commandMatchesQuery(command, commandQuery),
        )
  const roomMatches =
    roomQuery === null || roomCompletions === undefined
      ? []
      : roomCompletions(roomQuery)
  const mentionMatches =
    referenceQuery?.kind !== 'mention' || mentionCompletions === undefined
      ? []
      : mentionCompletions(referenceQuery.query)
  const roomReferenceMatches =
    referenceQuery?.kind !== 'room-reference' ||
    roomReferenceCompletions === undefined
      ? []
      : roomReferenceCompletions(referenceQuery.query)
  const emojiMatches =
    emojiQuery === null || emojiCompletions === undefined
      ? []
      : emojiCompletions(emojiQuery.query)
  // One menu at a time: the command names while the name is still being typed,
  // the room matches once `/room ` has moved on to its argument. Both the open
  // state and the rendered kind come off this single candidate, so an Escape
  // that dismisses the menu can't be answered by the other kind reopening it.
  const candidate =
    commandQuery !== null && commandMatches.length > 0
      ? {
          kind: 'command' as const,
          query: commandQuery,
          length: commandMatches.length,
        }
      : roomQuery !== null && roomMatches.length > 0
        ? {
            kind: 'room' as const,
            query: roomQuery,
            length: roomMatches.length,
          }
        : referenceQuery?.kind === 'mention' && mentionMatches.length > 0
          ? {
              kind: 'mention' as const,
              query: referenceQuery.query,
              length: mentionMatches.length,
            }
          : referenceQuery?.kind === 'room-reference' &&
              roomReferenceMatches.length > 0
            ? {
                kind: 'room-reference' as const,
                query: referenceQuery.query,
                length: roomReferenceMatches.length,
              }
            : emojiQuery !== null && emojiMatches.length > 0
              ? {
                  kind: 'emoji' as const,
                  query: emojiQuery.query,
                  length: emojiMatches.length,
                }
              : null
  const autocomplete =
    candidate !== null &&
    dismissKey(candidate.kind, candidate.query) !== autocompleteDismissedFor
      ? candidate
      : null
  const autocompleteOpen = autocomplete !== null
  const autocompleteKind = autocomplete?.kind ?? null
  const selectedAutocompleteCommand =
    commandMatches[Math.min(selectedCommand, commandMatches.length - 1)]
  const selectedRoomCompletion =
    roomMatches[Math.min(selectedCommand, roomMatches.length - 1)]
  const selectedMention =
    mentionMatches[Math.min(selectedCommand, mentionMatches.length - 1)]
  const selectedRoomReference =
    roomReferenceMatches[
      Math.min(selectedCommand, roomReferenceMatches.length - 1)
    ]
  const selectedEmoji =
    emojiMatches[Math.min(selectedCommand, emojiMatches.length - 1)]
  const exactCommand =
    commandQuery !== null &&
    commandMatches.some((command) => commandMatchesExact(command, commandQuery))
  const exactRoom =
    roomQuery !== null &&
    roomMatches.some(
      (room) => room.value.toLowerCase() === roomQuery.toLowerCase(),
    )
  const exactMention =
    referenceQuery?.kind === 'mention' &&
    mentionMatches.some(
      (member) =>
        member.value.toLowerCase() === `@${referenceQuery.query}`.toLowerCase(),
    )
  const exactRoomReference =
    referenceQuery?.kind === 'room-reference' &&
    roomReferenceMatches.some(
      (room) =>
        room.value.toLowerCase() === `#${referenceQuery.query}`.toLowerCase(),
    )
  const exactReactionEmoji =
    reactionEmojiQuery !== null &&
    emojiMatches.some((emoji) =>
      emojiOptionMatchesQuery(emoji, reactionEmojiQuery.query),
    )
  const autocompleteId = 'composer-slash-commands'
  const activeOptionId =
    autocompleteKind === 'command'
      ? selectedAutocompleteCommand === undefined
        ? undefined
        : slashCommandOptionId(selectedAutocompleteCommand)
      : autocompleteKind === 'room'
        ? selectedRoomCompletion === undefined
          ? undefined
          : roomCompletionOptionId(selectedRoomCompletion)
        : autocompleteKind === 'mention'
          ? selectedMention === undefined
            ? undefined
            : tokenCompletionOptionId('mention', selectedMention)
          : autocompleteKind === 'room-reference'
            ? selectedRoomReference === undefined
              ? undefined
              : tokenCompletionOptionId('room-reference', selectedRoomReference)
            : autocompleteKind === 'emoji'
              ? selectedEmoji === undefined
                ? undefined
                : tokenCompletionOptionId('emoji', selectedEmoji)
              : undefined

  // Adopt a late-arriving `initialValue` — a draft hydrated after mount, or one
  // pushed from another device, including that device clearing it (`''`). Only
  // a "clean" field adopts: empty, or still holding exactly the value we last
  // synced. Anything else is an unsynced local edit we must not clobber. This
  // is the TUI's `buffer_clean` rule (app/drafts.rs), so the two clients agree.
  useEffect(() => {
    if (initialValue === synced.current) {
      return
    }
    const clean = draft === '' || draft === synced.current
    synced.current = initialValue
    if (clean) {
      setDraft(initialValue)
    }
  }, [initialValue, draft])

  // Focus when entering a reply/edit so typing can start immediately.
  useEffect(() => {
    if (banner !== undefined) {
      textarea.current?.focus()
    }
  }, [banner])

  useEffect(() => {
    if (autoFocus) {
      textarea.current?.focus()
    }
  }, [autoFocus])

  // Focus on request (ADR 0078). Only on a *change*: focusing on mount would
  // fight the narrow-layout heading focus RoomPage takes on room open.
  const focusedAt = useRef(focusRequest)
  useEffect(() => {
    if (focusRequest === focusedAt.current) {
      return
    }
    focusedAt.current = focusRequest
    textarea.current?.focus()
  }, [focusRequest])

  useEffect(() => {
    setSelectedCommand(0)
  }, [
    commandQuery,
    roomQuery,
    referenceQuery?.kind,
    referenceQuery?.query,
    reactionEmojiQuery?.query,
  ])

  useLayoutEffect(() => {
    const el = textarea.current
    if (el === null || currentComposerHeight !== null) {
      return
    }
    const resize = () => {
      if (!window.matchMedia(SINGLE_PANE_QUERY).matches) {
        el.style.removeProperty('height')
        el.style.removeProperty('overflow-y')
        return
      }
      const style = getComputedStyle(el)
      const lineHeight = cssPixelValue(style.lineHeight, COMPOSER_MIN_HEIGHT)
      const padding =
        cssPixelValue(style.paddingTop, 0) +
        cssPixelValue(style.paddingBottom, 0)
      const maxHeight = lineHeight * MOBILE_COMPOSER_MAX_LINES + padding
      el.style.height = 'auto'
      if (draft === '') {
        const minHeight = lineHeight + padding
        el.style.height = `${Math.min(minHeight, maxHeight)}px`
        el.style.overflowY = 'hidden'
        return
      }
      const nextHeight = Math.min(el.scrollHeight, maxHeight)
      el.style.height = `${nextHeight}px`
      el.style.overflowY = el.scrollHeight > maxHeight ? 'auto' : 'hidden'
    }
    resize()
    window.addEventListener('resize', resize)
    return () => window.removeEventListener('resize', resize)
  }, [currentComposerHeight, draft])

  useEffect(() => {
    if (activeOptionId === undefined) {
      return
    }
    document
      .getElementById(activeOptionId)
      ?.scrollIntoView?.({ block: 'nearest' })
  }, [activeOptionId])

  const change = (value: string, nextCaret = value.length) => {
    setDraft(value)
    setCaret(nextCaret)
    setAutocompleteDismissedFor(null)
    if (!commandsEnabled || !isSlashCommandDraft(value)) {
      onDraftChange?.(value)
    }
  }

  const completeCommand = (command: SlashCommandSpec | undefined) => {
    if (command === undefined) {
      return
    }
    setDraft(`${command.name} `)
    setCaret(command.name.length + 1)
    setAutocompleteDismissedFor(null)
    textarea.current?.focus()
  }

  const completeRoom = (room: ComposerAutocompleteOption | undefined) => {
    if (room === undefined) {
      return
    }
    setDraft(`${SLASH_COMMAND.room} ${room.value} `)
    setCaret(SLASH_COMMAND.room.length + room.value.length + 2)
    setAutocompleteDismissedFor(dismissKey('room', room.value))
    textarea.current?.focus()
  }

  const completeToken = (
    option: ComposerAutocompleteOption | undefined,
    kind: 'mention' | 'room-reference',
  ) => {
    if (option === undefined || referenceQuery?.kind !== kind) {
      return
    }
    const suffix = draft.slice(referenceQuery.end)
    const spacer = suffix === '' || !/^[\s.,!?;:)\]}]/.test(suffix) ? ' ' : ''
    const next = `${draft.slice(0, referenceQuery.start)}${option.value}${spacer}${suffix}`
    const nextCaret = referenceQuery.start + option.value.length + spacer.length
    change(next, nextCaret)
    setAutocompleteDismissedFor(dismissKey(kind, option.value))
    textarea.current?.focus()
    requestAnimationFrame(() => {
      textarea.current?.setSelectionRange(nextCaret, nextCaret)
    })
  }

  const completeEmoji = (option: ComposerAutocompleteOption | undefined) => {
    if (option === undefined || emojiQuery === null) {
      return
    }
    const suffix = draft.slice(emojiQuery.end)
    const spacer = suffix === '' || !/^[\s.,!?;:)\]}]/.test(suffix) ? ' ' : ''
    const next = `${draft.slice(0, emojiQuery.start)}${option.value}${spacer}${suffix}`
    const nextCaret = emojiQuery.start + option.value.length + spacer.length
    change(next, nextCaret)
    setAutocompleteDismissedFor(dismissKey('emoji', option.value))
    textarea.current?.focus()
    requestAnimationFrame(() => {
      textarea.current?.setSelectionRange(nextCaret, nextCaret)
    })
  }

  const completeAutocomplete = () => {
    if (autocompleteKind === 'room') {
      completeRoom(selectedRoomCompletion)
    } else if (autocompleteKind === 'command') {
      completeCommand(selectedAutocompleteCommand)
    } else if (autocompleteKind === 'mention') {
      completeToken(selectedMention, 'mention')
    } else if (autocompleteKind === 'room-reference') {
      completeToken(selectedRoomReference, 'room-reference')
    } else if (autocompleteKind === 'emoji') {
      completeEmoji(selectedEmoji)
    }
  }

  const submit = () => {
    const body = draft.trim()
    // A staged file is itself a message: sending it bare, with no caption, is
    // the common case, so an empty draft is only inert when nothing is attached.
    if (body === '' && (attachments?.items.length ?? 0) === 0) {
      return
    }
    if (commandsEnabled && isSlashCommandDraft(body)) {
      const result = onCommand?.(body)
      if (isPromiseLike(result)) {
        void result
          .then((handled) => {
            if (handled) {
              setDraft('')
              onDraftChange?.('')
            }
          })
          .catch(() => {})
      } else if (result === true) {
        setDraft('')
        onDraftChange?.('')
      }
      return
    }
    // `//` escapes a literal leading slash — but only where a bare `/` would
    // have meant a command. A composer without commands sends both verbatim.
    const message =
      commandsEnabled && body.startsWith('//') ? body.slice(1) : body
    setDraft('')
    onDraftChange?.('')
    void onSubmit(message)
  }

  const clampComposerHeight = (height: number): number =>
    Math.round(
      Math.min(
        Math.max(height, COMPOSER_MIN_HEIGHT),
        Math.max(160, window.innerHeight * 0.5),
      ),
    )

  const setHeight = (next: number | null) => {
    if (height === undefined) {
      setComposerHeight(next)
    }
    onHeightChange?.(next)
  }

  const beginComposerResize = (
    event: JSX.TargetedPointerEvent<HTMLButtonElement>,
  ) => {
    event.preventDefault()
    const startY = event.clientY
    const startHeight = textarea.current?.getBoundingClientRect().height ?? 0
    event.currentTarget.setPointerCapture(event.pointerId)

    const resize = (move: PointerEvent) => {
      setHeight(clampComposerHeight(startHeight + startY - move.clientY))
    }
    const stop = () => {
      window.removeEventListener('pointermove', resize)
      window.removeEventListener('pointerup', stop)
      window.removeEventListener('pointercancel', stop)
    }
    window.addEventListener('pointermove', resize)
    window.addEventListener('pointerup', stop)
    window.addEventListener('pointercancel', stop)
  }

  const resizeComposerByKeyboard = (delta: number) => {
    const current = textarea.current?.getBoundingClientRect().height
    if (current === undefined) {
      return
    }
    setHeight(clampComposerHeight(current + delta))
  }

  return (
    <div class="composer">
      {status !== undefined && (
        <div class="composer-status" role="status" aria-live="polite">
          {status}
        </div>
      )}
      {banner !== undefined && (
        <div class="composer-banner">
          <span>
            <strong>{banner.label}</strong>{' '}
            <span class="muted">{banner.excerpt}</span>
          </span>
          <button type="button" class="ghost" onClick={banner.onCancel}>
            Cancel
          </button>
        </div>
      )}
      {attachments !== undefined && attachments.items.length > 0 && (
        <>
          {attachments.items.length === 1 ? (
            /* One file keeps the original chip verbatim, so the
               single-attachment path looks and behaves exactly as before. */
            <div class="composer-attachment">
              {attachments.items[0].previewUrl !== null && (
                <img
                  class="composer-attachment-preview"
                  src={attachments.items[0].previewUrl!}
                  alt=""
                />
              )}
              <span class="composer-attachment-name">
                {attachments.items[0].file.name}
              </span>
              <span class="muted">
                {humanSize(attachments.items[0].file.size)}
              </span>
              <button
                type="button"
                class="ghost"
                aria-label={`Remove ${attachments.items[0].file.name}`}
                onClick={() => attachments.onRemove(attachments.items[0].id)}
              >
                ✕
              </button>
            </div>
          ) : (
            <div class="composer-attachment-strip">
              <ul
                class="composer-attachment-list"
                aria-label={`${attachments.items.length} files to send`}
              >
                {attachments.items.map((item, index) => (
                  <li key={item.id} class="composer-attachment-item">
                    {item.previewUrl !== null ? (
                      <img
                        class="composer-attachment-thumb"
                        src={item.previewUrl}
                        alt=""
                      />
                    ) : (
                      <span class="composer-attachment-thumb muted">
                        {item.file.name}
                      </span>
                    )}
                    <button
                      type="button"
                      class="ghost composer-attachment-remove"
                      // Position as well as name: staging is additive and
                      // keyed by id, so two pastes of the same screenshot are
                      // two items with one name. Named alone, they would be
                      // indistinguishable to a screen reader. This branch only
                      // renders for two or more, so the position is never
                      // noise — the single-file chip keeps the bare name.
                      aria-label={`Remove file ${index + 1} of ${
                        attachments.items.length
                      }, ${item.file.name}`}
                      onClick={() => attachments.onRemove(item.id)}
                    >
                      ✕
                    </button>
                    {index === 0 && (
                      <span
                        class="composer-attachment-first"
                        aria-hidden="true"
                      >
                        1
                      </span>
                    )}
                  </li>
                ))}
              </ul>
              <p class="muted composer-attachment-summary">
                {attachments.items.length} files ·{' '}
                {humanSize(attachments.totalBytes)} — your message captions the
                first attachment
              </p>
            </div>
          )}
          {attachments.skipped > 0 && (
            <p class="muted composer-attachment-skipped" role="status">
              {attachments.skipped}{' '}
              {attachments.skipped === 1 ? 'file' : 'files'} not added —{' '}
              {attachments.skippedReason === 'size'
                ? 'the batch would be too large'
                : 'too many at once'}
            </p>
          )}
        </>
      )}
      {autocompleteOpen && (
        <div
          id={autocompleteId}
          class="slash-command-menu"
          role="listbox"
          aria-label={
            autocompleteKind === 'mention'
              ? 'User matches'
              : autocompleteKind === 'emoji'
                ? 'Emoji matches'
                : autocompleteKind === 'room' ||
                    autocompleteKind === 'room-reference'
                  ? 'Room matches'
                  : 'Slash commands'
          }
        >
          {autocompleteKind === 'command'
            ? commandMatches.map((command) => {
                const selected = command === selectedAutocompleteCommand
                return (
                  <button
                    id={slashCommandOptionId(command)}
                    key={command.name}
                    type="button"
                    role="option"
                    aria-selected={selected}
                    class={`slash-command-option${selected ? ' selected' : ''}`}
                    onMouseDown={(event) => {
                      event.preventDefault()
                      completeCommand(command)
                    }}
                  >
                    <span class="slash-command-name">{command.usage}</span>
                    <span class="slash-command-description">
                      {command.description}
                    </span>
                  </button>
                )
              })
            : (autocompleteKind === 'room'
                ? roomMatches
                : autocompleteKind === 'mention'
                  ? mentionMatches
                  : autocompleteKind === 'room-reference'
                    ? roomReferenceMatches
                    : emojiMatches
              ).map((room) => {
                const selected = room === selectedRoomCompletion
                const selectedToken =
                  room === selectedMention ||
                  room === selectedRoomReference ||
                  room === selectedEmoji
                const isSelected =
                  autocompleteKind === 'room' ? selected : selectedToken
                return (
                  <button
                    id={
                      autocompleteKind === 'room'
                        ? roomCompletionOptionId(room)
                        : tokenCompletionOptionId(autocompleteKind!, room)
                    }
                    key={`${room.value}\u0000${room.description}`}
                    type="button"
                    role="option"
                    aria-selected={isSelected}
                    class={`slash-command-option${isSelected ? ' selected' : ''}`}
                    onMouseDown={(event) => {
                      event.preventDefault()
                      if (autocompleteKind === 'room') {
                        completeRoom(room)
                      } else if (autocompleteKind === 'emoji') {
                        completeEmoji(room)
                      } else {
                        completeToken(room, autocompleteKind!)
                      }
                    }}
                  >
                    <span class="slash-command-name">{room.label}</span>
                    <span class="slash-command-description">
                      {room.description}
                    </span>
                  </button>
                )
              })}
        </div>
      )}
      <form
        onSubmit={(event) => {
          event.preventDefault()
          submit()
        }}
      >
        <div class="composer-input-shell">
          <button
            type="button"
            class="composer-resize"
            aria-label="Resize message input"
            title="Drag up to resize"
            onPointerDown={beginComposerResize}
            onDblClick={() => setHeight(null)}
            onKeyDown={(event) => {
              if (event.key === 'ArrowUp') {
                event.preventDefault()
                resizeComposerByKeyboard(COMPOSER_KEYBOARD_RESIZE_STEP)
              } else if (event.key === 'ArrowDown') {
                event.preventDefault()
                resizeComposerByKeyboard(-COMPOSER_KEYBOARD_RESIZE_STEP)
              } else if (event.key === 'Home') {
                event.preventDefault()
                setHeight(null)
              }
            }}
          />
          <textarea
            ref={textarea}
            style={
              currentComposerHeight === null
                ? undefined
                : { height: `${currentComposerHeight}px` }
            }
            value={draft}
            placeholder={placeholder}
            aria-label={ariaLabel}
            aria-autocomplete="list"
            aria-controls={autocompleteOpen ? autocompleteId : undefined}
            aria-expanded={autocompleteOpen}
            aria-activedescendant={activeOptionId}
            rows={1}
            enterkeyhint={singlePane ? 'enter' : 'send'}
            onInput={(event) =>
              change(
                event.currentTarget.value,
                event.currentTarget.selectionStart,
              )
            }
            onClick={(event) => setCaret(event.currentTarget.selectionStart)}
            onPointerDown={(event) => {
              if (event.pointerType === 'touch') {
                event.currentTarget.dataset.touchFocus = ''
              } else {
                delete event.currentTarget.dataset.touchFocus
              }
            }}
            onFocus={onFocus}
            onBlur={(event) => {
              delete event.currentTarget.dataset.touchFocus
            }}
            onKeyUp={(event) => setCaret(event.currentTarget.selectionStart)}
            onSelect={(event) => setCaret(event.currentTarget.selectionStart)}
            onPaste={(event) => {
              // A pasted screenshot arrives as a file on the clipboard. Claim
              // the event *only* when one is actually there — an ordinary text
              // paste must still land in the field.
              //
              // The whole list is handed over, N or 1. Measured in Chromium: a
              // web-page multi-image copy delivers *no* files at all (ADR
              // 0081), so this rarely sees more than one — but the staging
              // hook takes whatever arrives and the caps live there.
              const pastedFiles = event.clipboardData?.files
              if (
                pastedFiles !== undefined &&
                pastedFiles.length > 0 &&
                onAttach !== undefined
              ) {
                event.preventDefault()
                onAttach(pastedFiles)
                return
              }
              const pasted = event.clipboardData?.getData?.('text/plain') ?? ''
              const start = event.currentTarget.selectionStart
              const end = event.currentTarget.selectionEnd
              if (start === end) {
                return
              }
              const selected = event.currentTarget.value.slice(start, end)
              const link = markdownLinkForSelection(selected, pasted)
              if (link === null) {
                return
              }
              event.preventDefault()
              const next = `${event.currentTarget.value.slice(0, start)}${link}${event.currentTarget.value.slice(end)}`
              const nextCaret = start + link.length
              change(next, nextCaret)
              requestAnimationFrame(() => {
                textarea.current?.setSelectionRange(nextCaret, nextCaret)
              })
            }}
            onKeyDown={(event) => {
              const wholeMessageCaret =
                autocomplete === null ? wholeMessageCaretTarget(event) : null
              if (wholeMessageCaret !== null) {
                event.preventDefault()
                moveWholeMessageCaret(event.currentTarget, wholeMessageCaret)
                setCaret(wholeMessageCaret)
                return
              }
              // Resize from the composer itself (ADR 0078 keyboard reach): the
              // drag grip's arrow keys only work once it has focus, and its tab
              // position is not consistent across browsers. `mod+shift` + a
              // punctuation key is browser-safe and — unlike any arrow chord —
              // does not fight the textarea's native caret/selection editing.
              // Matched on `event.key` (the character the active layout emits,
              // shifted to `>`/`<`/`)`), not `event.code`, so the chord follows
              // the user's keyboard layout — Dvorak included — rather than the
              // physical QWERTY key position. This is what the app's `chordOf`
              // does too. The unshifted forms are accepted as a belt-and-braces
              // fallback for layouts that don't shift these to the glyph.
              const resizeMod =
                (event.ctrlKey || event.metaKey) && event.shiftKey
              if (resizeMod && (event.key === '.' || event.key === '>')) {
                event.preventDefault()
                resizeComposerByKeyboard(COMPOSER_KEYBOARD_RESIZE_STEP)
              } else if (
                resizeMod &&
                (event.key === ',' || event.key === '<')
              ) {
                event.preventDefault()
                resizeComposerByKeyboard(-COMPOSER_KEYBOARD_RESIZE_STEP)
              } else if (
                resizeMod &&
                (event.key === '0' || event.key === ')')
              ) {
                event.preventDefault()
                setHeight(null)
              } else if (
                autocomplete !== null &&
                (event.key === 'ArrowDown' || event.key === 'ArrowUp')
              ) {
                event.preventDefault()
                const count = autocomplete.length
                setSelectedCommand((current) =>
                  event.key === 'ArrowDown'
                    ? (current + 1) % count
                    : (current + count - 1) % count,
                )
              } else if (autocompleteOpen && event.key === 'Tab') {
                event.preventDefault()
                completeAutocomplete()
              } else if (
                autocompleteOpen &&
                event.key === 'Enter' &&
                !event.shiftKey &&
                !exactCommand &&
                !exactRoom &&
                !exactMention &&
                !exactRoomReference &&
                !exactReactionEmoji
              ) {
                event.preventDefault()
                completeAutocomplete()
              } else if (autocomplete !== null && event.key === 'Escape') {
                event.preventDefault()
                setAutocompleteDismissedFor(
                  dismissKey(autocomplete.kind, autocomplete.query),
                )
              } else if (
                event.key === 'Enter' &&
                !event.shiftKey &&
                !event.isComposing &&
                !singlePane
              ) {
                event.preventDefault()
                submit()
              } else if (event.key === 'Escape' && banner !== undefined) {
                // Claim the Escape so the staged handler in RoomPage doesn't
                // also close the thread panel behind this banner.
                event.preventDefault()
                banner.onCancel()
              } else if (
                event.key === 'ArrowUp' &&
                // Bare `↑` only: modified arrows are platform text navigation
                // or room stepping, and claiming one here would hide it from
                // the document-level shortcut layer.
                !hasModifier(event) &&
                draft === '' &&
                banner === undefined &&
                onEditLast !== undefined
              ) {
                event.preventDefault()
                onEditLast()
              }
            }}
          />
        </div>
        {onAttach !== undefined && (
          <>
            <input
              ref={fileInput}
              type="file"
              multiple
              class="composer-file-input"
              aria-label="Attach a file"
              onChange={(event) => {
                const picked = event.currentTarget.files
                if (picked !== null && picked.length > 0) {
                  onAttach(picked)
                }
                // Clear the input, so picking the same file twice in a row
                // still fires `change` the second time.
                event.currentTarget.value = ''
              }}
            />
            {/* A mouse affordance that proxies clicks to the input above —
                which is the real, labelled control, and the one a keyboard or
                screen reader reaches. Hidden from the a11y tree so "Attach a
                file" names exactly one thing. */}
            <button
              type="button"
              class="ghost composer-attach"
              title="Attach a file"
              aria-hidden="true"
              tabIndex={-1}
              onClick={() => fileInput.current?.click()}
            >
              <svg
                class="composer-attach-icon"
                viewBox="0 0 24 24"
                aria-hidden="true"
              >
                <path
                  d="M21.44 11.05 12.25 20.24a6 6 0 1 1-8.49-8.48l10.6-10.61a4 4 0 0 1 5.66 5.66L9.4 17.43a2 2 0 0 1-2.83-2.83l9.9-9.9"
                  fill="none"
                  stroke="currentColor"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  stroke-width="2"
                />
              </svg>
            </button>
          </>
        )}
        <button
          type="submit"
          class="composer-send"
          aria-label="Send"
          title="Send"
          disabled={
            draft.trim() === '' && (attachments?.items.length ?? 0) === 0
          }
        >
          <svg
            class="composer-send-icon"
            viewBox="0 0 24 24"
            aria-hidden="true"
          >
            <path
              d="M5 12h14M13 6l6 6-6 6"
              fill="none"
              stroke="currentColor"
              stroke-linecap="round"
              stroke-linejoin="round"
              stroke-width="2"
            />
          </svg>
          <span class="composer-send-label">Send</span>
        </button>
      </form>
    </div>
  )
}

function isSlashCommandDraft(value: string): boolean {
  return value.startsWith('/') && !value.startsWith('//')
}

function slashCommandQuery(value: string): string | null {
  if (!isSlashCommandDraft(value)) {
    return null
  }
  const trimmedStart = value.trimStart()
  if (/\s/.test(trimmedStart)) {
    return null
  }
  return trimmedStart
}

function isPromiseLike<T>(
  value: T | PromiseLike<T> | undefined,
): value is PromiseLike<T> {
  return (
    value !== undefined &&
    typeof value === 'object' &&
    value !== null &&
    'then' in value &&
    typeof value.then === 'function'
  )
}

function roomCommandQuery(value: string): string | null {
  if (!isSlashCommandDraft(value)) {
    return null
  }
  const trimmedStart = value.trimStart()
  const rest = trimmedStart.slice(SLASH_COMMAND.room.length)
  if (
    !trimmedStart.startsWith(SLASH_COMMAND.room) ||
    (rest !== '' && !/^\s/.test(rest))
  ) {
    return null
  }
  return rest.trim()
}

function reactCommandEmojiQuery(
  value: string,
  caret: number,
): {
  kind: 'emoji'
  query: string
  start: number
  end: number
} | null {
  if (!isSlashCommandDraft(value)) {
    return null
  }
  const before = value.slice(0, caret)
  const match = /^(\/\S+)\s+(:?[A-Za-z0-9_+-]+)$/.exec(before)
  if (match === null) {
    return null
  }
  const commandName = canonicalSlashCommandName(match[1])
  if (commandName !== SLASH_COMMAND.react) {
    return null
  }
  const token = match[2]
  const query = token.startsWith(':') ? token.slice(1) : token
  if (query === '') {
    return null
  }
  return {
    kind: 'emoji',
    query,
    start: caret - token.length,
    end: caret,
  }
}

function slashCommandOptionId(command: SlashCommandSpec): string {
  return `composer-slash-command-${command.name.slice(1)}`
}

function commandMatchesQuery(
  command: SlashCommandSpec,
  query: string,
): boolean {
  const normalized = query.toLowerCase()
  return [command.name, ...(command.aliases ?? [])].some((candidate) =>
    candidate.toLowerCase().startsWith(normalized),
  )
}

function commandMatchesExact(
  command: SlashCommandSpec,
  query: string,
): boolean {
  const normalized = query.toLowerCase()
  return [command.name, ...(command.aliases ?? [])].some(
    (candidate) => candidate.toLowerCase() === normalized,
  )
}

function roomCompletionOptionId(room: ComposerAutocompleteOption): string {
  return `composer-room-${encodeURIComponent(room.value)}`
}

/** What an Escape dismisses: this menu for this query, not the query alone. */
function dismissKey(kind: AutocompleteKind, query: string): string {
  return `${kind}\u0000${query}`
}

function tokenCompletionOptionId(
  kind: 'mention' | 'room-reference' | 'emoji',
  option: ComposerAutocompleteOption,
): string {
  return `composer-${kind}-${encodeURIComponent(option.value)}`
}

function emojiOptionMatchesQuery(
  option: ComposerAutocompleteOption,
  query: string,
): boolean {
  return (
    option.value.replace(/^:/, '').replace(/:$/, '').toLowerCase() ===
    query.toLowerCase()
  )
}

function wholeMessageCaretTarget(
  event: JSX.TargetedKeyboardEvent<HTMLTextAreaElement>,
): number | null {
  if (event.altKey || event.shiftKey) {
    return null
  }
  const apple = isApplePlatform()
  const start =
    apple &&
    event.metaKey &&
    !event.ctrlKey &&
    (event.key === 'ArrowUp' || event.key === 'Home')
  const end =
    apple &&
    event.metaKey &&
    !event.ctrlKey &&
    (event.key === 'ArrowDown' || event.key === 'End')
  const windowsStart =
    !apple && event.ctrlKey && !event.metaKey && event.key === 'Home'
  const windowsEnd =
    !apple && event.ctrlKey && !event.metaKey && event.key === 'End'

  if (start || windowsStart) {
    return 0
  }
  if (end || windowsEnd) {
    return event.currentTarget.value.length
  }
  return null
}

function moveWholeMessageCaret(
  element: HTMLTextAreaElement,
  target: number,
): void {
  const move = () => {
    element.setSelectionRange(target, target)
    element.scrollTop = target === 0 ? 0 : element.scrollHeight
  }
  move()
  requestAnimationFrame(move)
}

function referenceTokenQuery(
  value: string,
  caret: number,
): {
  kind: 'mention' | 'room-reference' | 'emoji'
  query: string
  start: number
  end: number
} | null {
  const before = value.slice(0, caret)
  const match =
    /(^|\s)([@#][^\s@#]*)$/.exec(before) ??
    /(^|\s)(:[A-Za-z0-9_+-]+)$/.exec(before)
  if (match === null) {
    return null
  }
  const token = match[2]
  const trigger = token[0]
  if (trigger === ':') {
    return {
      kind: 'emoji',
      query: token.slice(1),
      start: caret - token.length,
      end: caret,
    }
  }
  return {
    kind: trigger === '@' ? 'mention' : 'room-reference',
    query: token.slice(1),
    start: caret - token.length,
    end: caret,
  }
}
