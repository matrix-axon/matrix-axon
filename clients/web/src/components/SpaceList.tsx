import type { VNode } from 'preact'
import { useLayoutEffect, useRef } from 'preact/hooks'
import { ErrorBanner } from './ErrorBanner'
import { SINGLE_PANE_QUERY, useMediaQuery } from '../layout'
import { useMediaBlob } from '../media/use-media-blob'
import { useServices } from '../services'
import { hint, KEYS, keyAria } from '../shortcuts'
import { roomKey, roomTitle, type RoomDto } from '../stores/room-list'
import { spacePickerEntries, type SpacePickerEntry } from '../stores/spaces'

/** The space picker scopes the neighboring RoomList; it never changes Matrix state. */
export function SpaceList() {
  const { rooms, spaces, settings, spaceOrder } = useServices()
  const dragging = useRef<string | null>(null)
  const selected = spaces.selected.value
  const entries = spacePickerEntries(
    spaces,
    settings.spaceOrder.value,
    rooms.titles.value,
  )
  const order = entries
    .filter(({ depth }) => depth === 0)
    .map(({ space }) => roomKey(space))
  const rootRanks = new Map(order.map((key, index) => [key, index]))
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

  const showNames = settings.showSpaceNames.value
  const childrenByPath = new Map<string | null, SpacePickerEntry[]>()
  let maxDepth = 1
  for (const entry of entries) {
    const siblings = childrenByPath.get(entry.parentPath) ?? []
    siblings.push(entry)
    childrenByPath.set(entry.parentPath, siblings)
    maxDepth = Math.max(maxDepth, entry.depth)
  }

  function renderBranches(parent: string | null): VNode[] | undefined {
    return childrenByPath
      .get(parent)
      ?.map(({ space, path, depth, expandable }) => {
        const key = roomKey(space)
        const index = rootRanks.get(key)
        const expanded = spaces.expanded.value.has(path)
        const children = renderBranches(path)
        const title = roomTitle(space, rooms.titles.value)
        const unread = spaces.unread.value.get(key) ?? 0
        const move = (toIndex: number) => {
          if (toIndex < 0 || toIndex >= order.length) return
          spaceOrder.move(key, toIndex, order)
        }
        return (
          <div class="space-picker-branch" key={path}>
            <div
              class={
                depth === 0
                  ? 'space-picker-row'
                  : 'space-picker-row space-picker-nested'
              }
              style={depth === 0 ? undefined : { '--space-depth': depth }}
              draggable={index !== undefined}
              onDragStart={(event) => {
                if (index === undefined) return
                dragging.current = key
                // Firefox refuses to start a drag unless `dragstart` sets data,
                // and iOS Safari (which runs this off a long press) wants it too.
                // Chromium does not, which is why the Chromium-only e2e run never
                // caught this.
                event.dataTransfer?.setData('text/plain', key)
              }}
              onDragEnd={() => (dragging.current = null)}
              onDragOver={(event) => {
                if (index !== undefined) event.preventDefault()
              }}
              onDrop={(event) => {
                if (index === undefined) return
                event.preventDefault()
                const source = dragging.current
                dragging.current = null
                if (source !== null) spaceOrder.move(source, index, order)
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
                data-space-key={index === undefined ? undefined : key}
                onClick={() => (spaces.selected.value = key)}
                // Reordering without any visible chrome: the arrows below stay
                // hidden until asked for, so a focused space also answers to
                // Alt-↑/↓ directly.
                onKeyDown={(event) => {
                  if (
                    index === undefined ||
                    !event.altKey ||
                    event.ctrlKey ||
                    event.metaKey
                  )
                    return
                  const back = horizontal ? 'ArrowLeft' : 'ArrowUp'
                  const forward = horizontal ? 'ArrowRight' : 'ArrowDown'
                  if (event.key !== back && event.key !== forward) return
                  event.preventDefault()
                  move(index + (event.key === back ? -1 : 1))
                  focusAfterMove.current = key
                }}
                title={
                  index === undefined
                    ? title
                    : `${title} (Alt-${horizontal ? '←/→' : '↑/↓'} to move)`
                }
                aria-label={unread > 0 ? `${title}, ${unread} unread` : title}
                aria-keyshortcuts={
                  index === undefined
                    ? undefined
                    : horizontal
                      ? 'Alt+ArrowLeft Alt+ArrowRight'
                      : 'Alt+ArrowUp Alt+ArrowDown'
                }
              >
                <SpaceAvatar
                  accountId={space.account_id}
                  room={space}
                  title={title}
                />
                <span class="space-picker-name">{title}</span>
                {unread > 0 && (
                  <span
                    class="badge unread-count space-unread"
                    aria-hidden="true"
                  >
                    {unread > 99 ? '99+' : unread}
                  </span>
                )}
              </button>
              {expandable && (
                <button
                  type="button"
                  class="space-picker-expand"
                  aria-expanded={expanded}
                  aria-label={`${expanded ? 'Collapse' : 'Expand'} ${title} subspaces`}
                  title={`${expanded ? 'Collapse' : 'Expand'} ${title} subspaces`}
                  onClick={() => spaces.setExpanded(path, !expanded)}
                >
                  <span aria-hidden="true">{expanded ? '▾' : '▸'}</span>
                </button>
              )}
              {/* Drag-and-drop is undiscoverable and uneven on touch, so pointer
                users get buttons too — but only on request, so the rail stays
                as compact as possible by default. */}
              {reordering && index !== undefined && (
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
                    disabled={index === order.length - 1}
                    onClick={() => move(index + 1)}
                    title={`Move ${title} ${axis.onward}`}
                    aria-label={`Move ${title} ${axis.onward}`}
                  >
                    {axis.forward}
                  </button>
                </div>
              )}
            </div>
            {children && (
              <div
                class="space-picker-children"
                style={{
                  '--space-depth': depth + 1,
                  '--space-line-hue': (212 + depth * 113) % 360,
                }}
              >
                <span class="space-picker-connector" aria-hidden="true" />
                {children}
              </div>
            )}
          </div>
        )
      })
  }

  return (
    <section
      id="spaces-pane"
      class={showNames ? 'space-picker show-space-names' : 'space-picker'}
      style={{ '--space-indent': `${Math.min(0.24, 0.8 / maxDepth)}rem` }}
      aria-label="Spaces"
      ref={picker}
    >
      <ErrorBanner error={spaceOrder.error} />
      <h2>Spaces</h2>
      <div class="space-picker-tools">
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
          class="space-picker-names-toggle"
          aria-label="Show space names"
          aria-pressed={showNames}
          onClick={() => (settings.showSpaceNames.value = !showNames)}
          title={showNames ? 'Hide space names' : 'Show space names'}
        >
          <span aria-hidden="true">Aa</span>
        </button>
      </div>
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
      {renderBranches(null)}
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
