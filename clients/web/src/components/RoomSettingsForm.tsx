import { useEffect, useRef, useState } from 'preact/hooks'
import { inBackground } from '../api/client'
import { useFileDrop } from '../media/use-file-drop'
import { useServices } from '../services'
import type { RoomDto } from '../stores/room-list'
import type { RoomSettingsResult } from '../stores/rooms'
import { RoomAvatar } from './RoomAvatar'

/**
 * An avatar ceiling well below `MAX_UPLOAD_BYTES` (100 MiB). That cap is sized
 * for arbitrary media sends; a room avatar is displayed at ~56px and every
 * member downloads it, so a multi-megabyte original is nobody's intent. The
 * bytes are sent unresized (matching media-send) — this only refuses the
 * absurd, it does not silently re-encode what the user chose.
 */
const MAX_AVATAR_BYTES = 8 * 1024 * 1024

/** What the user changed, so only genuinely dirty fields are written. */
interface Draft {
  name: string
  topic: string
  /** `undefined` = leave the avatar alone; `null` = remove it. */
  avatar: File | null | undefined
}

export interface RoomSettingsFormProps {
  accountId: string
  roomId: string
  room: RoomDto | undefined
  displayTitle: string
  avatarMxc: string | null
  avatarColor: number
  /** Closes edit mode. `status` is the sentence to show in the panel. */
  onDone: (status: string | null) => void
  /** Reports whether anything is unsaved, so Close can confirm before discarding. */
  onDirtyChange: (dirty: boolean) => void
  /** Cancel pressed with unsaved changes — the panel confirms, then unmounts. */
  onRequestCancel: () => void
  /** An image chosen elsewhere (the avatar viewer) for this form to validate. */
  initialAvatar?: File | null
  /** Called once `initialAvatar` has been consumed, so it is not re-applied. */
  onAvatarTaken?: () => void
  /** True when the panel is displaying a DM peer's picture in place of one. */
  peerAvatarShown?: boolean
  /**
   * Whether a save is in flight. The panel uses it to stop offering "Discard
   * changes" — once the requests are out, the changes are not unsaved, and a
   * discard prompt would be describing something that is already happening.
   */
  onSavingChange?: (saving: boolean) => void
}

/**
 * The form is only reachable once `room` has loaded — the edit gate derives
 * `ownUserId` from it — so the `undefined` fallbacks here are defensive, not a
 * path that would silently clear a field the user never saw.
 */
export function currentName(room: RoomDto | undefined): string {
  return room?.name ?? ''
}

export function currentTopic(room: RoomDto | undefined): string {
  return room?.topic ?? ''
}

/**
 * Validate a picked avatar before anything reaches the network. Both refusals
 * mirror a real server `400` observed against a live homeserver — "image
 * uploads must have an image/* content type" and "avatar upload must declare a
 * content type" — so this turns two round trips into an immediate message. A
 * file with no detectable type fails the same test, which is what makes the
 * second case unreachable from this client.
 */
export function avatarFileError(file: File): string | null {
  if (!file.type.startsWith('image/')) {
    return file.type === ''
      ? 'That file has no detectable type. Choose an image file.'
      : `A room avatar must be an image. That file is ${file.type}.`
  }
  if (file.size > MAX_AVATAR_BYTES) {
    return `That image is ${Math.round(file.size / (1024 * 1024))} MB. Choose one under 8 MB.`
  }
  return null
}

/**
 * The image on the clipboard, if there is one.
 *
 * `clipboardData.files` covers a file copied from a file manager; a
 * screenshot or an image copied from a web page arrives only as an *item*
 * with no entry in `files` on some engines, so both are checked. Returns
 * `null` for a text paste, which must be left alone to reach the field the
 * user was typing in.
 */
export function imageFromClipboard(event: ClipboardEvent): File | null {
  const data = event.clipboardData
  if (data === null) {
    return null
  }
  for (const file of Array.from(data.files)) {
    if (file.type.startsWith('image/')) {
      return file
    }
  }
  for (const item of Array.from(data.items)) {
    if (item.kind === 'file' && item.type.startsWith('image/')) {
      const file = item.getAsFile()
      if (file !== null) {
        return file
      }
    }
  }
  return null
}

/**
 * Whether these bytes actually decode as an image.
 *
 * `file.type` is derived from the file's *extension*, not its content, so a
 * text file renamed to `.jpg` arrives as `image/jpeg` and passes every cheap
 * check — including the server's, which also only inspects the declared type
 * (verified against a live Axon: a 30-byte ASCII file uploads fine). Without
 * this the avatar is set to bytes no client can render, and every member sees
 * a broken image with nothing having reported an error.
 *
 * Decoding is the only check that looks at the bytes, and it covers every
 * format the browser itself can display — no per-format magic-number table to
 * keep in step with what `accept="image/*"` lets through.
 */
export function decodeImage(url: string): Promise<boolean> {
  return new Promise((resolve) => {
    const image = new Image()
    image.onload = () =>
      resolve(image.naturalWidth > 0 && image.naturalHeight > 0)
    image.onerror = () => resolve(false)
    image.src = url
  })
}

export function RoomSettingsForm({
  accountId,
  roomId,
  room,
  displayTitle,
  avatarMxc,
  avatarColor,
  onDone,
  onDirtyChange,
  onRequestCancel,
  initialAvatar = null,
  onAvatarTaken,
  onSavingChange,
  peerAvatarShown = false,
}: RoomSettingsFormProps) {
  const { rooms, media } = useServices()
  const fileInput = useRef<HTMLInputElement>(null)
  /**
   * The room's name and topic as they were when this editor opened.
   *
   * Dirtiness is measured against this, never against the live `room` prop:
   * that prop keeps updating while the editor is open (a rename from another
   * client arrives over the socket and `noteTimelineEvent` patches it in).
   * Compared against the live value, a field the user never touched would
   * turn "dirty" the moment someone else changed it, and Save would write the
   * stale value back — silently reverting their change.
   */
  const [baseline] = useState(() => ({
    name: currentName(room),
    topic: currentTopic(room),
  }))
  const [draft, setDraft] = useState<Draft>({
    name: baseline.name,
    topic: baseline.topic,
    avatar: undefined,
  })
  const [preview, setPreview] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [checking, setChecking] = useState(false)
  const [error, setError] = useState<string | null>(null)

  /**
   * False once this form has gone away. A save is several awaits long, and
   * the panel keeps one instance per room: without this, a save started for
   * room A that lands after the user has switched to room B and started
   * editing would call the panel's `onDone` and wipe B's draft. The requests
   * themselves are still allowed to finish — they are A's, and correct.
   */
  const alive = useRef(true)
  useEffect(
    () => () => {
      alive.current = false
    },
    [],
  )

  // Revoking is tied to the URL's own lifetime rather than to any code path:
  // this cleanup runs both when `preview` is replaced and when the form
  // unmounts, and the panel can drop the form without going through
  // `cancel()` (a room switch, or a discarded edit). An object URL that
  // outlives its <img> is a leak nothing else cleans up.
  useEffect(() => {
    if (preview === null) {
      return
    }
    return () => URL.revokeObjectURL(preview)
  }, [preview])

  const nameDirty = draft.name !== baseline.name
  const topicDirty = draft.topic !== baseline.topic
  const avatarDirty = draft.avatar !== undefined
  const dirty = nameDirty || topicDirty || avatarDirty

  // Derived from the draft rather than pushed from each edit site: the avatar
  // path now sets the draft *after* an await, where the `draft` captured by
  // the handler's closure may already be stale.
  useEffect(() => {
    onDirtyChange(dirty)
  }, [dirty, onDirtyChange])

  const update = (patch: Partial<Draft>, nextPreview?: string | null) => {
    setDraft((current) => ({ ...current, ...patch }))
    setError(null)
    if (nextPreview !== undefined) {
      setPreview(nextPreview)
    }
  }

  /** Let the same file be re-picked after the user swaps it out. */
  const resetPicker = () => {
    if (fileInput.current !== null) fileInput.current.value = ''
  }

  /**
   * The one path a candidate avatar takes, whichever way it arrived — the
   * picker, a drop, or a paste. Keeping the validation here rather than at
   * each entry point is what stops drag-and-drop quietly skipping the decode
   * check the picker does.
   */
  const acceptFile = async (file: File) => {
    const invalid = avatarFileError(file)
    if (invalid !== null) {
      setError(invalid)
      resetPicker()
      return
    }
    setError(null)
    setChecking(true)
    const url = URL.createObjectURL(file)
    const readable = await decodeImage(url)
    if (!alive.current) {
      URL.revokeObjectURL(url)
      return
    }
    setChecking(false)
    if (!readable) {
      URL.revokeObjectURL(url)
      setError(
        `${file.name} is named like an image but its contents could not be ` +
          `read as one. It may be corrupt, or renamed from another format.`,
      )
      resetPicker()
      return
    }
    update({ avatar: file }, url)
  }

  const pickAvatar = (event: Event) => {
    // Read the file synchronously: `currentTarget` is null after an await.
    const file = (event.currentTarget as HTMLInputElement).files?.[0]
    if (file !== undefined) {
      void acceptFile(file)
    }
  }

  const { dragging, handlers: dropHandlers } = useFileDrop((files) => {
    // An avatar is one image; the first dropped file is the least surprising
    // reading of a multi-file drop onto a single-value field.
    const file = files[0]
    if (file !== undefined && !busy && !checking) {
      void acceptFile(file)
    }
  })

  const pasteAvatar = (event: ClipboardEvent) => {
    if (busy || checking) {
      return
    }
    const file = imageFromClipboard(event)
    if (file === null) {
      // Not an image — let the paste reach the name/topic field normally.
      return
    }
    event.preventDefault()
    void acceptFile(file)
  }

  // A file picked in the avatar viewer arrives here rather than being written
  // from there, so it goes through exactly the same decode and size checks as
  // one chosen with this form's own picker, and lands as a preview awaiting
  // Save rather than as an immediate write.
  const taken = useRef(false)
  useEffect(() => {
    if (initialAvatar === null || taken.current) {
      return
    }
    taken.current = true
    void acceptFile(initialAvatar).then(() => onAvatarTaken?.())
    // `acceptFile` is recreated every render; this must run once per handoff.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initialAvatar])

  const cancel = () => {
    if (dirty) {
      // The panel owns the confirm, so one dialog serves both this and Close.
      onRequestCancel()
      return
    }
    onDone(null)
  }

  const save = async (event: Event) => {
    event.preventDefault()
    if (!dirty) {
      cancel()
      return
    }
    setBusy(true)
    onSavingChange?.(true)
    setError(null)
    const saved: string[] = []
    const failed: string[] = []
    const note = (label: string, result: RoomSettingsResult): void => {
      if (result.ok) {
        saved.push(label)
        return
      }
      failed.push(
        result.code === 'forbidden'
          ? `${label} (you are not allowed to change it in this room)`
          : `${label} (${result.message})`,
      )
    }

    if (nameDirty) {
      note('name', await rooms.setRoomName(accountId, roomId, draft.name))
    }
    if (topicDirty) {
      note('topic', await rooms.setRoomTopic(accountId, roomId, draft.topic))
    }
    if (draft.avatar === null) {
      note('avatar', await rooms.removeRoomAvatar(accountId, roomId))
    } else if (draft.avatar !== undefined) {
      const staged = await media.upload(accountId, draft.avatar)
      if (staged.ok) {
        note(
          'avatar',
          await rooms.setRoomAvatar(accountId, roomId, staged.uploadId),
        )
      } else {
        failed.push('avatar (the image could not be uploaded)')
      }
    }
    // Both of these run whether or not this form still exists.
    //
    // The saving flag is the parent's, and the parent outlives the form: left
    // stuck `true` by an unmount it would disable the discard confirmation
    // for whatever room the user moved on to, silently dropping *that* room's
    // edit.
    //
    // The refresh is the socket-down fallback — without it a save that landed
    // while the socket was down shows nothing until an unrelated refresh
    // happens by. The writes succeeded regardless of who is still on screen,
    // so the room list should reflect them either way.
    onSavingChange?.(false)
    if (saved.length > 0) {
      // The authoritative update normally arrives as an `m.room.*` live frame
      // that `rooms.noteTimelineEvent` patches in; this only covers its
      // absence. Fire-and-forget: the form's own result does not depend on it.
      inBackground(rooms.refresh())
    }

    if (!alive.current) {
      // This form is gone — the user switched rooms, or closed the panel.
      // Only the *local* UI update is dropped: reporting an outcome into a
      // form that no longer exists, or onto whatever replaced it.
      return
    }
    setBusy(false)

    if (failed.length > 0) {
      // Partial success is reported as partial, never as success: some of
      // these writes may well have landed.
      const parts = [
        saved.length > 0 ? `Saved ${formatList(saved)}.` : null,
        `Could not save ${formatList(failed)}.`,
      ].filter((part): part is string => part !== null)
      setError(parts.join(' '))
      return
    }
    onDone(`Saved ${formatList(saved)}.`)
  }

  return (
    <form
      class="room-settings-form"
      onSubmit={(event) => void save(event)}
      onPaste={pasteAvatar}
      {...dropHandlers}
    >
      {dragging && (
        <div class="drop-overlay" aria-hidden="true">
          <p>Drop to set the avatar</p>
        </div>
      )}
      <div class="room-settings-avatar">
        <RoomAvatar
          accountId={accountId}
          mxcUrl={draft.avatar === null ? null : avatarMxc}
          previewUrl={preview}
          title={displayTitle}
          color={avatarColor}
        />
        <div class="room-settings-avatar-actions">
          {/* The repo's existing file-picker shape (see the QR image picker):
              a styled <label> wrapping a visually hidden input, so the label
              itself opens the picker with no click plumbing. Deliberately not
              the `hidden` attribute — that removes the control from the
              accessibility tree entirely. */}
          <label class="button-like room-settings-file">
            {checking ? 'Checking image…' : 'Change avatar…'}
            <input
              ref={fileInput}
              class="visually-hidden"
              type="file"
              accept="image/*"
              disabled={busy || checking}
              onChange={(event) => void pickAvatar(event)}
            />
          </label>
          <button
            type="button"
            class="ghost"
            disabled={
              busy || checking || (avatarMxc === null && draft.avatar == null)
            }
            onClick={() => update({ avatar: null }, null)}
          >
            Remove avatar
          </button>
        </div>
      </div>

      <label>
        Name
        <input
          type="text"
          value={draft.name}
          placeholder="No name"
          disabled={busy}
          onInput={(event) => update({ name: event.currentTarget.value })}
        />
      </label>
      <label>
        Topic
        <textarea
          rows={3}
          value={draft.topic}
          placeholder="No topic"
          disabled={busy}
          onInput={(event) => update({ topic: event.currentTarget.value })}
        />
      </label>
      {peerAvatarShown && (
        // Without this the avatar appears to vanish on opening the editor: the
        // panel shows the other member's picture for a DM with no room avatar,
        // and this form only ever shows the room's own.
        <p class="room-settings-hint muted">
          This conversation shows the other member&rsquo;s picture. Setting an
          avatar here replaces it for everyone in the room.
        </p>
      )}
      <p class="room-settings-hint muted">
        Drop or paste an image to set the avatar. Clearing a field removes it
        for everyone in the room.
      </p>

      {error !== null && (
        <p class="error" role="alert">
          {error}
        </p>
      )}

      <div class="dialog-actions">
        <button
          type="button"
          class="ghost"
          disabled={busy || checking}
          onClick={cancel}
        >
          Cancel
        </button>
        <button type="submit" disabled={busy || checking || !dirty}>
          {busy ? 'Saving…' : 'Save'}
        </button>
      </div>
    </form>
  )
}

function formatList(parts: readonly string[]): string {
  if (parts.length <= 2) {
    return parts.join(' and ')
  }
  return `${parts.slice(0, -1).join(', ')}, and ${parts[parts.length - 1]}`
}
