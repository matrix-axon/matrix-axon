import { act, cleanup, fireEvent, render } from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { NativeDrag } from '../platform'
import { preventStrayFileDrops, useFileDrop } from './use-file-drop'

afterEach(cleanup)

/** A drag payload the hook should recognize as carrying a file. */
const withFiles = (files: File[] = []) => ({
  dataTransfer: { types: ['Files'], files },
})
/** Dragging selected text, not a file — the hook must ignore it entirely. */
const withText = { dataTransfer: { types: ['text/plain'], files: [] } }

function Harness({
  onFile,
}: {
  onFile?: (files: FileList | readonly File[]) => void
}) {
  const { dragging, problem, handlers } = useFileDrop(onFile ?? (() => {}))
  return (
    <div data-testid="pane" {...handlers}>
      {problem !== null && !dragging && <p role="alert">{problem}</p>}
      {dragging && <span data-testid="overlay">Drop to attach</span>}
      <span data-testid="child">a timeline row</span>
    </div>
  )
}

describe('useFileDrop (ADR 0065)', () => {
  const png = () => new File(['bytes'], 'cat.png', { type: 'image/png' })

  it('stays armed while the cursor crosses child elements', () => {
    const { getByTestId, queryByTestId } = render(<Harness />)
    const pane = getByTestId('pane')
    const child = getByTestId('child')

    fireEvent.dragEnter(pane, withFiles())
    expect(queryByTestId('overlay')).not.toBeNull()

    // Entering a child fires `dragenter` on it *and* `dragleave` on the parent.
    // Counting enters against leaves is what stops the overlay flickering off
    // as the cursor moves across the timeline's rows.
    fireEvent.dragEnter(child, withFiles())
    fireEvent.dragLeave(pane, withFiles())
    expect(queryByTestId('overlay')).not.toBeNull()

    // Only the final leave, back out of the pane, disarms it.
    fireEvent.dragLeave(child, withFiles())
    expect(queryByTestId('overlay')).toBeNull()
  })

  it('ignores a drag that carries no file', () => {
    const onFile = vi.fn()
    const { getByTestId, queryByTestId } = render(<Harness onFile={onFile} />)
    const pane = getByTestId('pane')

    fireEvent.dragEnter(pane, withText)
    expect(queryByTestId('overlay')).toBeNull()

    fireEvent.drop(pane, withText)
    expect(onFile).not.toHaveBeenCalled()
  })

  it('passes every file of a multi-file drop through', () => {
    // ADR 0065 took only the first and reported the rest as skipped; ADR 0081
    // sends them all. The caps live in the staging hook, so deciding here what
    // to drop would put that rule in two places and let them disagree.
    const onFile = vi.fn()
    const { getByTestId, queryByTestId } = render(<Harness onFile={onFile} />)
    const pane = getByTestId('pane')
    const first = png()
    const rest = [png(), png()]

    fireEvent.dragEnter(pane, withFiles([first]))
    fireEvent.drop(pane, withFiles([first, ...rest]))

    expect(onFile).toHaveBeenCalledTimes(1)
    expect([...onFile.mock.calls[0][0]]).toHaveLength(3)
    // The overlay clears on drop, not on some later leave that never comes.
    expect(queryByTestId('overlay')).toBeNull()
  })

  it('re-arms cleanly for a second drag after a drop', () => {
    const { getByTestId, queryByTestId } = render(<Harness />)
    const pane = getByTestId('pane')

    fireEvent.dragEnter(pane, withFiles())
    fireEvent.drop(pane, withFiles([png()]))
    // A drop resets the counter; a stale count would leave the next drag needing
    // two leaves to disarm.
    fireEvent.dragEnter(pane, withFiles())
    expect(queryByTestId('overlay')).not.toBeNull()

    fireEvent.dragLeave(pane, withFiles())
    expect(queryByTestId('overlay')).toBeNull()
  })
})

/** WebKitGTK advertises this for a file-manager drag; `Files` may be absent. */
const withUriList = { dataTransfer: { types: ['text/uri-list'], files: [] } }

describe('a drag WebKitGTK reports as text/uri-list', () => {
  it('is intercepted, so the composer does not receive a pasted path', () => {
    // The reported Linux symptom: dropping an image on the message box typed
    // its path in. The guard required `Files` in `types`, so the handler bailed
    // and the textarea's own default ran.
    const onFile = vi.fn()
    const { getByTestId } = render(<Harness onFile={onFile} />)

    const prevented = !fireEvent.drop(getByTestId('pane'), withUriList)

    expect(prevented).toBe(true)
    // Nothing to stage from a bare URI, and that is fine — doing nothing beats
    // pasting a path nobody typed.
    expect(onFile).not.toHaveBeenCalled()
  })

  it('is also intercepted on dragover, or the drop never fires', () => {
    const { getByTestId } = render(<Harness />)

    expect(!fireEvent.dragOver(getByTestId('pane'), withUriList)).toBe(true)
  })
})

describe('preventStrayFileDrops', () => {
  it('refuses a file dropped outside any drop target', () => {
    // Otherwise the browser navigates to the file and the app is replaced by
    // the image, with no way back but a restart.
    const stop = preventStrayFileDrops(document)

    expect(!fireEvent.drop(document.body, withFiles())).toBe(true)

    stop()
  })

  it('leaves a drag carrying no file alone', () => {
    // Dragging selected text, or a link, is not ours to interfere with.
    const stop = preventStrayFileDrops(document)

    expect(!fireEvent.drop(document.body, withText)).toBe(false)

    stop()
  })

  it('stops listening when disposed', () => {
    const stop = preventStrayFileDrops(document)
    stop()

    expect(!fireEvent.drop(document.body, withFiles())).toBe(false)
  })
})

describe('a drop that carries nothing usable', () => {
  it('says so instead of doing nothing', () => {
    // WebKitGTK advertises `text/uri-list` for a file-manager drag and can
    // hand over no `File` at all. Silence reads as the app being broken —
    // which is exactly how it was reported.
    const { getByTestId, queryByRole } = render(<Harness />)

    fireEvent.drop(getByTestId('pane'), withUriList)

    expect(queryByRole('alert')?.textContent).toMatch(/carried no file/i)
  })

  it('clears the message when a new drag starts', () => {
    const { getByTestId, queryByRole } = render(<Harness />)
    fireEvent.drop(getByTestId('pane'), withUriList)
    expect(queryByRole('alert')).not.toBeNull()

    fireEvent.dragEnter(getByTestId('pane'), withFiles())

    expect(queryByRole('alert')).toBeNull()
  })
})

describe('a drag the OS reports to the window (Linux)', () => {
  /**
   * The shell's channel, driven by hand. `subscribe` is what a pane is given
   * as `nativeDrops`; `deliver` plays the shell's part.
   */
  function nativeChannel() {
    const handlers = new Set<(drag: NativeDrag) => void>()
    return {
      subscribe: (handler: (drag: NativeDrag) => void) => {
        handlers.add(handler)
        return () => handlers.delete(handler)
      },
      // `act` because this is the shell calling in from outside Preact —
      // nothing here is an event the test library already wraps.
      deliver: (drag: NativeDrag) => {
        act(() => {
          for (const handler of [...handlers]) {
            handler(drag)
          }
        })
      },
      get listeners() {
        return handlers.size
      },
    }
  }

  /**
   * jsdom has no `elementFromPoint` — it does no layout — so the point-to-pane
   * mapping is stubbed. `closest` walking up to the tagged pane is real DOM,
   * and that is the part the hook actually owns.
   */
  function elementsAt(byX: Record<number, () => Element | null>) {
    // Assigned rather than spied: jsdom does not define the method at all, so
    // there is nothing for `vi.spyOn` to replace.
    const target = document as unknown as Record<string, unknown>
    const original = target.elementFromPoint
    target.elementFromPoint = (x: number) => byX[x]?.() ?? null
    return () => {
      target.elementFromPoint = original
    }
  }

  function TwoPanes({
    subscribe,
    onFile,
  }: {
    subscribe: (handler: (drag: NativeDrag) => void) => () => void
    onFile: (files: FileList | readonly File[]) => void
  }) {
    const room = useFileDrop(onFile, subscribe)
    const thread = useFileDrop(onFile, subscribe)
    return (
      <>
        <div data-testid="room" {...room.handlers}>
          {room.dragging && <span data-testid="room-overlay">Drop</span>}
          {room.problem !== null && <p role="alert">{room.problem}</p>}
          <span data-testid="room-child">a timeline row</span>
        </div>
        <div data-testid="thread" {...thread.handlers}>
          {thread.dragging && <span data-testid="thread-overlay">Drop</span>}
        </div>
      </>
    )
  }

  const png = () => new File(['bytes'], 'cat.png', { type: 'image/png' })

  it('arms only the pane the cursor is actually over', () => {
    const channel = nativeChannel()
    const { getByTestId, queryByTestId } = render(
      <TwoPanes subscribe={channel.subscribe} onFile={() => {}} />,
    )
    const restore = elementsAt({
      10: () => getByTestId('room-child'),
      90: () => getByTestId('thread'),
    })

    // The event carries a point and no target — it never went through the
    // DOM — so without the hit-test both panes would light up at once.
    channel.deliver({ kind: 'over', x: 10, y: 5 })
    expect(queryByTestId('room-overlay')).not.toBeNull()
    expect(queryByTestId('thread-overlay')).toBeNull()

    channel.deliver({ kind: 'over', x: 90, y: 5 })
    expect(queryByTestId('room-overlay')).toBeNull()
    expect(queryByTestId('thread-overlay')).not.toBeNull()

    restore()
  })

  it('stages into the pane under the cursor and no other', () => {
    const channel = nativeChannel()
    const staged = vi.fn()
    const { getByTestId } = render(
      <TwoPanes subscribe={channel.subscribe} onFile={staged} />,
    )
    const restore = elementsAt({ 90: () => getByTestId('thread') })

    channel.deliver({ kind: 'drop', x: 90, y: 5, files: [png()] })

    // One call, not two: a file dropped on the thread panel must not also
    // stage into the room's composer (ADR 0065).
    expect(staged).toHaveBeenCalledTimes(1)
    expect(staged.mock.calls[0]?.[0]).toHaveLength(1)

    restore()
  })

  it('ignores a drop that landed on neither pane', () => {
    const channel = nativeChannel()
    const staged = vi.fn()
    render(<TwoPanes subscribe={channel.subscribe} onFile={staged} />)
    // The sidebar, the topbar, empty space: `elementFromPoint` finds nothing
    // tagged as a drop target.
    const restore = elementsAt({})

    channel.deliver({ kind: 'drop', x: 5, y: 5, files: [png()] })

    expect(staged).not.toHaveBeenCalled()
    restore()
  })

  it('says so when every dropped path failed to read', () => {
    const channel = nativeChannel()
    const { getByTestId, queryByRole } = render(
      <TwoPanes subscribe={channel.subscribe} onFile={() => {}} />,
    )
    const restore = elementsAt({ 10: () => getByTestId('room-child') })

    channel.deliver({ kind: 'drop', x: 10, y: 5, files: [] })

    expect(queryByRole('alert')?.textContent).toMatch(/carried no file/i)
    restore()
  })

  it('disarms when the drag leaves the window', () => {
    const channel = nativeChannel()
    const { getByTestId, queryByTestId } = render(
      <TwoPanes subscribe={channel.subscribe} onFile={() => {}} />,
    )
    const restore = elementsAt({ 10: () => getByTestId('room-child') })

    channel.deliver({ kind: 'over', x: 10, y: 5 })
    expect(queryByTestId('room-overlay')).not.toBeNull()

    // A cancelled drag has no position at all, so there is nothing to test —
    // every pane must disarm.
    channel.deliver({ kind: 'leave' })
    expect(queryByTestId('room-overlay')).toBeNull()

    restore()
  })

  it('unsubscribes when the pane unmounts', () => {
    const channel = nativeChannel()
    const { unmount } = render(
      <TwoPanes subscribe={channel.subscribe} onFile={() => {}} />,
    )
    expect(channel.listeners).toBe(2)

    unmount()

    expect(channel.listeners).toBe(0)
  })
})
