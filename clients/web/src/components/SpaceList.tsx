import { useLayoutEffect, useMemo, useRef } from 'preact/hooks'
import { SINGLE_PANE_QUERY, useMediaQuery } from '../layout'
import { useMediaBlob } from '../media/use-media-blob'
import { useServices } from '../services'
import { hint, KEYS, keyAria } from '../shortcuts'
import { roomKey, roomTitle, type RoomDto } from '../stores/room-list'
import { orderedSpaces } from '../stores/spaces'

/** The space picker scopes the neighboring RoomList; it never changes Matrix state. */
export function SpaceList() {
  const { rooms, spaces, settings } = useServices()
  const dragging = useRef<string | null>(null)
  const selected = spaces.selected.value
  const entries = useMemo(
    () =>
      orderedSpaces(
        rooms.rooms.value.filter((room) => room.room_type === 'm.space'),
        settings.spaceOrder.value,
        rooms.titles.value,
      ),
    [rooms.rooms.value, settings.spaceOrder.value, rooms.titles.value],
  )
  // The order the user sees, which is what a move target indexes into.
  const order = useMemo(() => entries.map(roomKey), [entries])
  const reordering = spaces.reordering.value
  // The picker is a vertical rail beside the room list, but a horizontal strip
  // above it in single-pane mode, where "up" and "down" would name the wrong
  // axis. The controls are the same two buttons either way — only the axis they
  // describe changes.
  const horizontal = useMediaQuery(SINGLE_PANE_QUERY)
  const axis = horizontal
    ? { back: '◀', forward: '▶', backward: 'left', onward: 'right' }
    : { back: '▲', forward: '▼', backward: 'up', onward: 'down' }
  // Preact moves the keyed row rather than rebuilding it, but a moved node can
  // still lose focus; restoring it keeps Alt-↓ repeatable.
  const focusAfterMove = useRef<string | null>(null)
  const picker = useRef<HTMLElement>(null)
  useLayoutEffect(() => {
    const key = focusAfterMove.current
    if (key === null) return
    focusAfterMove.current = null
    // Room keys carry `/`, `!` and `:`, so match on the property rather than
    // building a selector out of one.
    const moved = [
      ...(picker.current?.querySelectorAll<HTMLButtonElement>(
        '[data-space-key]',
      ) ?? []),
    ].find((button) => button.dataset.spaceKey === key)
    moved?.focus()
  })

  return (
    <section
      id="spaces-pane"
      class="space-picker"
      aria-label="Spaces"
      ref={picker}
    >
      <h2>Spaces</h2>
      {/* Present at every width: a phone has no Ctrl-Alt-R, and touch
          drag-and-drop is uneven across engines and undiscoverable where it
          does work. */}
      <button
        type="button"
        class={
          reordering
            ? 'space-picker-reorder-toggle active'
            : 'space-picker-reorder-toggle'
        }
        aria-pressed={reordering}
        aria-controls="spaces-pane"
        onClick={() => (spaces.reordering.value = !reordering)}
        title={hint(
          reordering ? 'Done reordering spaces' : 'Reorder spaces',
          KEYS.reorderSpaces,
        )}
        aria-label={reordering ? 'Done reordering spaces' : 'Reorder spaces'}
        aria-keyshortcuts={keyAria(KEYS.reorderSpaces)}
      >
        <span aria-hidden="true">⇅</span>
      </button>
      <button
        type="button"
        class={
          selected === null ? 'space-picker-entry active' : 'space-picker-entry'
        }
        aria-pressed={selected === null}
        onClick={() => (spaces.selected.value = null)}
        title="All rooms"
        aria-label="All rooms"
      >
        All
      </button>
      {entries.map((space, index) => {
        const key = roomKey(space)
        const title = roomTitle(space, rooms.titles.value)
        const move = (toIndex: number) => {
          if (toIndex < 0 || toIndex >= order.length) return
          settings.moveSpace(key, toIndex, order)
        }
        return (
          <div
            class="space-picker-row"
            key={key}
            draggable
            onDragStart={(event) => {
              dragging.current = key
              // Firefox refuses to start a drag unless `dragstart` sets data,
              // and iOS Safari (which runs this off a long press) wants it too.
              // Chromium does not, which is why the Chromium-only e2e run never
              // caught this.
              event.dataTransfer?.setData('text/plain', key)
            }}
            onDragOver={(event) => event.preventDefault()}
            onDrop={(event) => {
              event.preventDefault()
              const source = dragging.current
              dragging.current = null
              if (source !== null) settings.moveSpace(source, index, order)
            }}
          >
            <button
              type="button"
              class={
                selected === key
                  ? 'space-picker-entry active'
                  : 'space-picker-entry'
              }
              aria-pressed={selected === key}
              data-space-key={key}
              onClick={() => (spaces.selected.value = key)}
              // Reordering without any visible chrome: the arrows below stay
              // hidden until asked for, so a focused space also answers to
              // Alt-↑/↓ directly.
              onKeyDown={(event) => {
                if (!event.altKey || event.ctrlKey || event.metaKey) return
                const back = horizontal ? 'ArrowLeft' : 'ArrowUp'
                const forward = horizontal ? 'ArrowRight' : 'ArrowDown'
                if (event.key !== back && event.key !== forward) return
                event.preventDefault()
                move(index + (event.key === back ? -1 : 1))
                focusAfterMove.current = key
              }}
              title={`${title} (Alt-${horizontal ? '←/→' : '↑/↓'} to move)`}
              aria-label={title}
              aria-keyshortcuts={
                horizontal
                  ? 'Alt+ArrowLeft Alt+ArrowRight'
                  : 'Alt+ArrowUp Alt+ArrowDown'
              }
            >
              <SpaceAvatar
                accountId={space.account_id}
                room={space}
                title={title}
              />
            </button>
            {/* Drag-and-drop is undiscoverable and uneven on touch, so pointer
                users get buttons too — but only on request, so the rail stays
                as compact as possible by default. */}
            {reordering && (
              <div class="space-picker-reorder">
                <button
                  type="button"
                  class="space-picker-move"
                  disabled={index === 0}
                  onClick={() => move(index - 1)}
                  title={`Move ${title} ${axis.backward}`}
                  aria-label={`Move ${title} ${axis.backward}`}
                >
                  {axis.back}
                </button>
                <button
                  type="button"
                  class="space-picker-move"
                  disabled={index === entries.length - 1}
                  onClick={() => move(index + 1)}
                  title={`Move ${title} ${axis.onward}`}
                  aria-label={`Move ${title} ${axis.onward}`}
                >
                  {axis.forward}
                </button>
              </div>
            )}
          </div>
        )
      })}
    </section>
  )
}

function SpaceAvatar({
  accountId,
  room,
  title,
}: {
  accountId: string
  room: RoomDto
  title: string
}) {
  const { ref, state } = useMediaBlob<HTMLSpanElement>(
    accountId,
    room.avatar_url ?? null,
  )
  const initial = title.trim().slice(0, 1).toLocaleUpperCase() || '?'
  return (
    <span ref={ref} class="space-avatar" aria-hidden="true">
      {state.status === 'ready' && state.url !== undefined ? (
        <img src={state.url} alt="" />
      ) : (
        initial
      )}
    </span>
  )
}
