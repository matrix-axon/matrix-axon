import type { ApiClient } from '../api/client'
import { useRef, useState } from 'preact/hooks'
import type { CustomEmoji } from '../emoji-packs'
import { useShortcuts } from '../shortcuts'
import { BodyPortal } from './BodyPortal'
import { ImagePackPicker } from './CustomEmojiPicker'
import { useModalFocus } from './use-modal-focus'

export interface StickerPickerOptions {
  api: ApiClient
  accountId: string
  roomId: string
  onSelect: (image: CustomEmoji) => void
}

/** Separate from the draft: selecting a sticker never submits composer text. */
export function StickerPickerButton(options: StickerPickerOptions) {
  const [open, setOpen] = useState(false)
  const button = useRef<HTMLButtonElement>(null)
  return (
    <>
      <button
        ref={button}
        type="button"
        class="ghost composer-attach composer-sticker"
        aria-label="Send sticker"
        title="Send sticker"
        onClick={() => setOpen(true)}
      >
        <svg
          class="composer-attach-icon"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          aria-hidden="true"
        >
          <path d="M20 13V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h8zM13 20v-5a2 2 0 0 1 2-2h5M7 8h2M13 8h2M7 13c1.5 2 3 2 4 1" />
        </svg>
      </button>
      {open && (
        <BodyPortal>
          <StickerDialog
            {...options}
            restoreTo={() => button.current}
            onClose={() => setOpen(false)}
            onSelect={(image) => {
              setOpen(false)
              options.onSelect(image)
            }}
          />
        </BodyPortal>
      )}
    </>
  )
}

function StickerDialog({
  accountId,
  roomId,
  onSelect,
  onClose,
  restoreTo,
}: StickerPickerOptions & {
  onClose: () => void
  restoreTo: () => HTMLElement | null
}) {
  const { containerRef } = useModalFocus<HTMLDivElement>({
    restoreTo,
    initialFocus: (): HTMLElement | null =>
      containerRef.current?.querySelector<HTMLInputElement>(
        'input[type="search"]',
      ) ?? null,
  })
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onClose()
      },
    },
    { whileTyping: true, capture: true },
  )
  return (
    <div
      class="overlay"
      role="dialog"
      aria-modal="true"
      aria-label="Sticker picker"
      ref={containerRef}
    >
      <div class="overlay-panel sticker-picker-panel">
        <div class="overlay-head">
          <h2>Stickers</h2>
          <button type="button" class="ghost" onClick={onClose}>
            Close
          </button>
        </div>
        <ImagePackPicker
          accountId={accountId}
          roomId={roomId}
          usage="sticker"
          onSelect={onSelect}
        />
      </div>
    </div>
  )
}
