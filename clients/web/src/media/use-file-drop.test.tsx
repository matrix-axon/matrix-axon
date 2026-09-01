import { cleanup, fireEvent, render } from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { preventStrayFileDrops, useFileDrop } from './use-file-drop'

afterEach(cleanup)

/** A drag payload the hook should recognize as carrying a file. */
const withFiles = (files: File[] = []) => ({
  dataTransfer: { types: ['Files'], files },
})
/** Dragging selected text, not a file — the hook must ignore it entirely. */
const withText = { dataTransfer: { types: ['text/plain'], files: [] } }

function Harness({ onFile }: { onFile?: (files: FileList) => void }) {
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
