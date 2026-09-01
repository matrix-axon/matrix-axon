import { useCallback, useEffect, useId, useRef, useState } from 'preact/hooks'
import type { Platform } from '../platform'

/**
 * Drag-and-drop of a file onto a pane (ADR 0065). Scoped to the element it is
 * spread onto, not the window: the thread panel sits *beside* the room stream,
 * and a page-wide drop target would stage a file dropped on the thread into the
 * room's composer — sending it to the wrong place.
 *
 * A drop stages the file; it does not send. `dragging` drives the drop overlay.
 *
 * `dragenter`/`dragleave` fire for every child element crossed, so a plain
 * boolean flickers off as the cursor moves over the timeline's rows. Counting
 * enters against leaves is the usual fix.
 *
 * `nativeDrops` is the second, mutually exclusive channel. Where a shell takes
 * the drag at the *window* — which is what the Linux build must do, because
 * WebKitGTK hands the page a URI and no bytes — none of the DOM handlers below
 * ever fire, and the drag arrives as a position and a set of already-read
 * files instead. Both channels drive the same `dragging`, `problem` and
 * `onFiles`, so everything downstream is unaware of which one ran.
 */
/**
 * Shown when a drop was accepted and yielded nothing to stage. Silence is
 * indistinguishable from the app being broken, which is how it was first
 * reported.
 */
const NOTHING_TO_STAGE =
  'That drop carried no file this app can read. Use the paperclip to choose it instead.'

/**
 * Whether a viewport point lies within the pane tagged `id`.
 *
 * `elementFromPoint` rather than a bounding box, so an overlapping pane
 * resolves the way the user sees it: the topmost element at the point wins,
 * which is what a DOM drop event would have given for free. `id` comes from
 * `useId` and contains nothing that needs escaping in a selector.
 */
function isOverTarget(id: string, x: number, y: number): boolean {
  const element = document.elementFromPoint(x, y)
  return (
    element !== null && element.closest(`[data-drop-target="${id}"]`) !== null
  )
}

export function useFileDrop(
  onFiles: (files: FileList | readonly File[]) => void,
  options: {
    /** The shell's window-level channel, where the platform has one. */
    nativeDrops?: Platform['onNativeFileDrop']
    /**
     * Identity of the surface this pane composes for
     * (`useMessageComposer`'s `attachmentScope`). Only read to drop a stale
     * `problem`: these panes are reused across a room change rather than
     * remounted, so without it the message follows the user into a room they
     * never dropped anything on.
     */
    scope?: string
  } = {},
): {
  dragging: boolean
  /**
   * Set when a drop was accepted but carried nothing that could be staged, and
   * cleared by the next drag. A drag can advertise `text/uri-list` and then
   * hand over no `File` at all — WebKitGTK does this for a file-manager drag —
   * and the honest outcome is to say so. Silently doing nothing reads as the
   * app being broken, which is how it was reported.
   */
  problem: string | null
  /**
   * Spread onto the element this drop is scoped to. Carries a `data-drop-target`
   * attribute as well as the DOM handlers: a native drag knows only *where* it
   * landed, never on what, so the attribute is how the point is resolved back
   * to a pane. Panes that spread nothing (an edit in progress, where media
   * cannot be attached) are correctly invisible to both channels at once.
   */
  handlers: {
    'data-drop-target': string
    onDragEnter(event: DragEvent): void
    onDragOver(event: DragEvent): void
    onDragLeave(event: DragEvent): void
    onDrop(event: DragEvent): void
  }
} {
  const { nativeDrops, scope } = options
  const [dragging, setDragging] = useState(false)
  const [problem, setProblem] = useState<string | null>(null)
  const depth = useRef(0)
  const targetId = useId()

  // Held in a ref so the native subscription below does not tear down and
  // rebuild on every render — `onFiles` is a fresh closure each time, and
  // resubscribing mid-drag would lose the drag.
  const latestOnFiles = useRef(onFiles)
  latestOnFiles.current = onFiles

  /**
   * Whether this drag looks like it carries a file.
   *
   * `Files` is what a browser reports for a drag out of a file manager. But WebKitGTK also advertises `text/uri-list`
   * for the same gesture and does not always include `Files` in `types` — and
   * when this returned false, the handlers below bailed and the *browser's*
   * default ran instead: a drop on the composer inserted the file's path as
   * text, which is what a Linux user reported. Anything file-shaped is
   * therefore intercepted, even if nothing can be staged from it; doing
   * nothing is a much better outcome than pasting a path nobody typed.
   */
  const looksLikeFile = (event: DragEvent) => {
    const types = Array.from(event.dataTransfer?.types ?? [])
    return types.includes('Files') || types.includes('text/uri-list')
  }

  const reset = useCallback(() => {
    depth.current = 0
    setDragging(false)
  }, [])

  // A stale message must not outlive the surface it was about.
  useEffect(() => {
    setProblem(null)
  }, [scope])

  useEffect(() => {
    if (nativeDrops === undefined || nativeDrops === null) {
      return
    }
    return nativeDrops((drag) => {
      if (drag.kind === 'leave') {
        reset()
        return
      }
      // The event carries a point, not a target, because it never went through
      // the DOM. Without this test every pane would light up at once and a
      // file dropped on the thread panel would stage into the room's composer
      // — the exact confusion ADR 0065 scoped this hook to a pane to avoid.
      const over = isOverTarget(targetId, drag.x, drag.y)
      if (drag.kind === 'over') {
        setDragging(over)
        if (over) {
          setProblem(null)
        }
        return
      }
      reset()
      if (!over) {
        return
      }
      if (drag.files.length > 0) {
        latestOnFiles.current(drag.files)
        return
      }
      setProblem(NOTHING_TO_STAGE)
    })
  }, [nativeDrops, reset, targetId])

  return {
    dragging,
    problem,
    handlers: {
      'data-drop-target': targetId,
      onDragEnter(event) {
        if (!looksLikeFile(event)) {
          return
        }
        depth.current += 1
        setDragging(true)
        setProblem(null)
      },
      onDragOver(event) {
        if (!looksLikeFile(event)) {
          return
        }
        // Without this the browser navigates to the dropped file, and the drop
        // event may never fire at all.
        event.preventDefault()
      },
      onDragLeave(event) {
        if (!looksLikeFile(event)) {
          return
        }
        depth.current -= 1
        if (depth.current <= 0) {
          reset()
        }
      },
      onDrop(event) {
        if (!looksLikeFile(event)) {
          return
        }
        // Prevented before the staging decision, not after: even a drag we
        // cannot stage from must not reach the browser's default handling.
        event.preventDefault()
        reset()
        // The whole list goes through (ADR 0081). Only the staging hook knows
        // the caps, so deciding here what to drop would put that rule in two
        // places and let them disagree.
        const files = event.dataTransfer?.files
        if (files !== undefined && files.length > 0) {
          onFiles(files)
          return
        }
        setProblem(NOTHING_TO_STAGE)
      },
    },
  }
}

/**
 * Stop the browser acting on a file dropped anywhere the app does not handle.
 *
 * `useFileDrop` is scoped to a pane on purpose (see above), which leaves the
 * rest of the window — sidebar, topbar, empty space — with no handler at all,
 * and there the browser does what browsers do with a dropped file: it navigates
 * to it. The app is simply *replaced* by the image, with no way back but a
 * restart, which is what a Linux user hit.
 *
 * This is a guard, not a drop target: it never stages anything, it only refuses
 * the default. A drop on a real target still stages, because the pane's own
 * handler runs first — it is the event's target, and this listens at the
 * document.
 */
export function preventStrayFileDrops(target: Document = document): () => void {
  const looksLikeFile = (event: DragEvent) => {
    const types = Array.from(event.dataTransfer?.types ?? [])
    return types.includes('Files') || types.includes('text/uri-list')
  }
  const swallow = (event: DragEvent) => {
    if (looksLikeFile(event)) {
      event.preventDefault()
    }
  }
  target.addEventListener('dragover', swallow)
  target.addEventListener('drop', swallow)
  return () => {
    target.removeEventListener('dragover', swallow)
    target.removeEventListener('drop', swallow)
  }
}
