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

/**
 * WebKitGTK advertises this for a file-manager drag; `Files` may be absent, and
 * the list is `file:` URIs.
 */
const withUriList = {
  dataTransfer: {
    types: ['text/uri-list'],
    files: [],
    getData: (type: string) =>
      type === 'text/uri-list' ? 'file:///home/adam/cat.png\r\n' : '',
  },
}
/** A hyperlink dragged out of another tab: the same `text/uri-list`, no file. */
const withLink = {
  dataTransfer: {
    types: ['text/uri-list', 'text/plain'],
    files: [],
    getData: (type: string) =>
      type === 'text/uri-list' ? 'https://example.org/thread\r\n' : '',
  },
}

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

describe('a dragged hyperlink', () => {
  function WithComposer({ onFile }: { onFile: () => void }) {
    const { dragging, problem, handlers } = useFileDrop(onFile)
    return (
      <div data-testid="pane" {...handlers}>
        {problem !== null && !dragging && <p role="alert">{problem}</p>}
        {dragging && <span data-testid="overlay">Drop to attach</span>}
        <textarea data-testid="composer" />
      </div>
    )
  }

  it('is left to the composer, which inserts the URL', () => {
    // A link advertises `text/uri-list` exactly as a WebKitGTK file drag does.
    // Claiming it prevented the textarea's default, so dragging a URL into
    // the message box did nothing — in a browser too, not only the shell.
    const onFile = vi.fn()
    const { getByTestId, queryByRole, queryByTestId } = render(
      <WithComposer onFile={onFile} />,
    )
    // Before the drop nothing distinguishes the two, so the overlay arms; the
    // drop is where the answer is known.
    fireEvent.dragEnter(getByTestId('composer'), withLink)
    expect(queryByTestId('overlay')).not.toBeNull()

    const prevented = !fireEvent.drop(getByTestId('composer'), withLink)

    expect(prevented).toBe(false)
    expect(onFile).not.toHaveBeenCalled()
    // Not a failed file drop, so no message about one — and the overlay is
    // down.
    expect(queryByRole('alert')).toBeNull()
    expect(queryByTestId('overlay')).toBeNull()
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
    // Dragging selected text is not ours to interfere with.
    const stop = preventStrayFileDrops(document)

    expect(!fireEvent.drop(document.body, withText)).toBe(false)

    stop()
  })

  it('refuses a WebKitGTK file drag, which advertises only text/uri-list', () => {
    // Same replaced-app failure as `Files`; the URIs say it is a file.
    const stop = preventStrayFileDrops(document)

    expect(!fireEvent.drop(document.body, withUriList)).toBe(true)

    stop()
  })

  it('lets a link through to a textarea, and nowhere else', () => {
    const stop = preventStrayFileDrops(document)
    const composer = document.createElement('textarea')
    document.body.appendChild(composer)

    // The textarea inserting the URL is what the user dragged it there for.
    expect(!fireEvent.drop(composer, withLink)).toBe(false)
    // On the sidebar or empty chrome the browser would navigate to it, and the
    // app is gone exactly as it was for a dropped file.
    expect(!fireEvent.drop(document.body, withLink)).toBe(true)

    composer.remove()
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

describe('a message about a drop that staged nothing', () => {
  function Scoped({ scope }: { scope: string }) {
    const { problem, handlers } = useFileDrop(() => {}, { scope })
    return (
      <div data-testid="pane" {...handlers}>
        {problem !== null && <p role="alert">{problem}</p>}
      </div>
    )
  }

  it('does not follow the user into another room', () => {
    // These panes are reused across a room change rather than remounted, so
    // the message outlived the room it was about and appeared in one the user
    // had never dropped anything on.
    const { getByTestId, queryByRole, rerender } = render(
      <Scoped scope="acct\0!room-a" />,
    )
    fireEvent.drop(getByTestId('pane'), withUriList)
    expect(queryByRole('alert')).not.toBeNull()

    rerender(<Scoped scope="acct\0!room-b" />)

    expect(queryByRole('alert')).toBeNull()
  })

  it('survives a render that did not change the surface', () => {
    const { getByTestId, queryByRole, rerender } = render(
      <Scoped scope="acct\0!room-a" />,
    )
    fireEvent.drop(getByTestId('pane'), withUriList)

    rerender(<Scoped scope="acct\0!room-a" />)

    expect(queryByRole('alert')).not.toBeNull()
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
    const room = useFileDrop(onFile, { nativeDrops: subscribe })
    const thread = useFileDrop(onFile, { nativeDrops: subscribe })
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

  it('stages into the pane under the cursor and no other', async () => {
    const channel = nativeChannel()
    const staged = vi.fn()
    const { getByTestId } = render(
      <TwoPanes subscribe={channel.subscribe} onFile={staged} />,
    )
    const restore = elementsAt({ 90: () => getByTestId('thread') })
    const files = vi.fn(() => Promise.resolve([png()]))

    channel.deliver({ kind: 'drop', x: 90, y: 5, files })

    // One call, not two: a file dropped on the thread panel must not also
    // stage into the room's composer (ADR 0065).
    await vi.waitFor(() => expect(staged).toHaveBeenCalledTimes(1))
    expect(staged.mock.calls[0]?.[0]).toHaveLength(1)
    // And one read, by the pane that wanted it. Both panes see the event;
    // reading in each would pull every file over IPC twice and discard half.
    expect(files).toHaveBeenCalledTimes(1)

    restore()
  })

  it('ignores a drop that landed on neither pane, and does not read it', async () => {
    const channel = nativeChannel()
    const staged = vi.fn()
    render(<TwoPanes subscribe={channel.subscribe} onFile={staged} />)
    // The sidebar, the topbar, empty space: `elementFromPoint` finds nothing
    // tagged as a drop target.
    const restore = elementsAt({})
    const files = vi.fn(() => Promise.resolve([png()]))

    channel.deliver({ kind: 'drop', x: 5, y: 5, files })

    // Settle anything that was going to happen.
    await act(() => Promise.resolve())
    expect(staged).not.toHaveBeenCalled()
    // Nobody wanted the bytes, so nobody paid for them.
    expect(files).not.toHaveBeenCalled()
    restore()
  })

  it('says so when every dropped path failed to read', async () => {
    const channel = nativeChannel()
    const { getByTestId, queryByRole } = render(
      <TwoPanes subscribe={channel.subscribe} onFile={() => {}} />,
    )
    const restore = elementsAt({ 10: () => getByTestId('room-child') })

    channel.deliver({
      kind: 'drop',
      x: 10,
      y: 5,
      files: () => Promise.resolve([]),
    })

    await vi.waitFor(() =>
      expect(queryByRole('alert')?.textContent).toMatch(/carried no file/i),
    )
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
