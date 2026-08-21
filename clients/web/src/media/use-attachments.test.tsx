import { act, cleanup, render } from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { MAX_BATCH_FILES, useAttachments } from './use-attachments'
import {
  createAttachmentStaging,
  type AttachmentStaging,
} from './attachment-staging'
import { MAX_UPLOAD_BYTES } from './media-service'
import { downscalePreview } from './downscale-preview'

/**
 * jsdom has no canvas, so the real `downscalePreview` resolves `null` and the
 * swap path never runs. Mocked here so the tests that care about it can decide
 * *when* it resolves — the interesting case is "after the room changed".
 */
vi.mock('./downscale-preview', () => ({
  downscalePreview: vi.fn(async () => null),
}))
const downscale = vi.mocked(downscalePreview)

afterEach(cleanup)

function png(name = 'cat.png', size = 10): File {
  const file = new File(['x'], name, { type: 'image/png' })
  Object.defineProperty(file, 'size', { value: size })
  return file
}

type Api = ReturnType<typeof useAttachments>
function harness(
  scope = 'room-a',
  staging: AttachmentStaging = createAttachmentStaging(),
) {
  const api: { current: Api | null } = { current: null }
  function Probe({ scope: s }: { scope: string }) {
    api.current = useAttachments(s, staging)
    return null
  }
  const view = render(<Probe scope={scope} />)
  return {
    api,
    staging,
    /** Flush the render, so `batch` reflects the call just made. */
    run: (fn: (api: Api) => void) =>
      act(() => {
        fn(api.current!)
      }),
    setScope: (next: string) =>
      act(() => {
        view.rerender(<Probe scope={next} />)
      }),
    unmount: () =>
      act(() => {
        view.unmount()
      }),
  }
}

describe('useAttachments', () => {
  it('stages additively, so a paste can join a picked batch', () => {
    const { api, run } = harness()
    run((a) => a.stage([png('a.png')]))
    expect(api.current!.batch.items).toHaveLength(1)

    run((a) => a.stage([png('b.png'), png('c.png')]))
    expect(api.current!.batch.items.map((i) => i.file.name)).toEqual([
      'a.png',
      'b.png',
      'c.png',
    ])
  })

  it('gives each item a stable id, because a File is not a usable key', () => {
    // Two identical pastes are equal enough to collide; Preact would then pair
    // the wrong chip with the wrong file (WCR-01).
    const { api, run } = harness()
    run((a) => a.stage([png('same.png'), png('same.png')]))
    const [first, second] = api.current!.batch.items
    expect(first.id).not.toBe(second.id)
  })

  it('removes one without disturbing the others', () => {
    const { api, run } = harness()
    run((a) => a.stage([png('a.png'), png('b.png'), png('c.png')]))
    const target = api.current!.batch.items[1]

    run((a) => a.remove(target.id))

    expect(api.current!.batch.items.map((i) => i.file.name)).toEqual([
      'a.png',
      'c.png',
    ])
  })

  it('reports the accumulated size', () => {
    const { api, run } = harness()
    run((a) => a.stage([png('a.png', 100), png('b.png', 250)]))
    expect(api.current!.batch.totalBytes).toBe(350)
  })

  describe('caps', () => {
    it('refuses past the file count, and says so', () => {
      const { api, run } = harness()
      const many = Array.from({ length: MAX_BATCH_FILES + 3 }, (_, i) =>
        png(`${i}.png`),
      )
      run((a) => a.stage(many))

      expect(api.current!.batch.items).toHaveLength(MAX_BATCH_FILES)
      expect(api.current!.batch.skipped).toBe(3)
      expect(api.current!.batch.skippedReason).toBe('count')
    })

    it('counts against what is already staged, not just this call', () => {
      const { api, run } = harness()
      api.current!.stage(
        Array.from({ length: MAX_BATCH_FILES }, (_, i) => png(`${i}.png`)),
      )
      run((a) => a.stage([png('one-more.png')]))

      expect(api.current!.batch.items).toHaveLength(MAX_BATCH_FILES)
      expect(api.current!.batch.skipped).toBe(1)
    })

    it('refuses on the accumulated size, not per file', () => {
      // Two files each under the cap can be far over it together.
      const { api, run } = harness()
      const half = Math.floor(MAX_UPLOAD_BYTES * 0.6)
      run((a) => a.stage([png('a.png', half)]))
      run((a) => a.stage([png('b.png', half)]))

      expect(api.current!.batch.items.map((i) => i.file.name)).toEqual([
        'a.png',
      ])
      expect(api.current!.batch.skippedReason).toBe('size')
    })
  })

  describe('object url ownership', () => {
    it('revokes on remove', () => {
      const revoke = vi.spyOn(URL, 'revokeObjectURL')
      const { api, run } = harness()
      run((a) => a.stage([png('a.png')]))
      const url = api.current!.batch.items[0].previewUrl!

      run((a) => a.remove(api.current!.batch.items[0].id))

      expect(revoke).toHaveBeenCalledWith(url)
      revoke.mockRestore()
    })

    it('revokes every url on clear', () => {
      const revoke = vi.spyOn(URL, 'revokeObjectURL')
      const { api, run } = harness()
      run((a) => a.stage([png('a.png'), png('b.png'), png('c.png')]))
      const urls = api.current!.batch.items.map((i) => i.previewUrl!)

      run((a) => a.clear())

      for (const url of urls) {
        expect(revoke).toHaveBeenCalledWith(url)
      }
      revoke.mockRestore()
    })

    it('keeps the url alive across a room change, since the file survives', () => {
      // Retention replaced the clear-on-scope-change (issue #89); what must not
      // change is that the url stays paired with its file rather than leaking.
      const revoke = vi.spyOn(URL, 'revokeObjectURL')
      const { api, run, setScope } = harness('room-a')
      run((a) => a.stage([png('a.png')]))
      const url = api.current!.batch.items[0].previewUrl!

      setScope('room-b')

      expect(revoke).not.toHaveBeenCalledWith(url)

      setScope('room-a')

      expect(api.current!.batch.items[0].previewUrl).toBe(url)
      revoke.mockRestore()
    })

    it('keeps the url when the room view unmounts, releasing it on sign-out', () => {
      // Unmount is *not* a release point any more: `RoomPage` unmounts on every
      // route change away from a room, which on a phone is every room change.
      // `clearAll` (sign-out) is what ends a staged file's life.
      const revoke = vi.spyOn(URL, 'revokeObjectURL')
      const { api, run, unmount, staging } = harness()
      run((a) => a.stage([png('a.png')]))
      const url = api.current!.batch.items[0].previewUrl!

      unmount()
      expect(revoke).not.toHaveBeenCalledWith(url)

      staging.clearAll()

      expect(revoke).toHaveBeenCalledWith(url)
      revoke.mockRestore()
    })

    it('makes no preview for a non-image, and nothing to revoke', () => {
      const { api, run } = harness()
      const pdf = new File(['x'], 'notes.pdf', { type: 'application/pdf' })
      run((a) => a.stage([pdf]))
      expect(api.current!.batch.items[0].previewUrl).toBeNull()
    })
  })

  // Issue #89: the text draft always survived a room switch (ADR 0048), and
  // the file — the half that cannot be retyped — did not.
  describe('retention per scope', () => {
    it('gives a re-entered room its staged files back', () => {
      const { api, run, setScope } = harness('room-a')
      run((a) => a.stage([png('a.png')]))

      setScope('room-b')
      expect(api.current!.batch.items).toEqual([])

      setScope('room-a')
      expect(api.current!.batch.items.map((i) => i.file.name)).toEqual([
        'a.png',
      ])
    })

    it("never shows one room the other room's files", () => {
      // The guard the clear-on-switch used to provide: `scope` is what stops a
      // file staged in room A from being sent in room B, and it is resolved
      // during render — not in an effect, which would leave one frame where
      // the new room shows, and `Enter` would send, the previous room's batch.
      const { api, run, setScope } = harness('room-a')
      run((a) => a.stage([png('a.png')]))
      setScope('room-b')
      run((a) => a.stage([png('b.png')]))

      expect(api.current!.batch.items.map((i) => i.file.name)).toEqual([
        'b.png',
      ])

      setScope('room-a')
      expect(api.current!.batch.items.map((i) => i.file.name)).toEqual([
        'a.png',
      ])
    })

    it('sending empties only the room that sent', () => {
      const { api, run, setScope } = harness('room-a')
      run((a) => a.stage([png('a.png')]))
      setScope('room-b')
      run((a) => a.stage([png('b.png')]))

      // What the submit path calls once the files are handed to the send.
      run((a) => a.clear())

      expect(api.current!.batch.items).toEqual([])
      setScope('room-a')
      expect(api.current!.batch.items).toHaveLength(1)
    })

    it('retires the least recently visited room past the cap', () => {
      // File bytes, not events: the cap is what bounds memory here.
      const revoke = vi.spyOn(URL, 'revokeObjectURL')
      const { api, run, setScope } = harness('room-a')
      run((a) => a.stage([png('a.png')]))
      const url = api.current!.batch.items[0].previewUrl!

      for (const scope of ['room-b', 'room-c', 'room-d']) {
        setScope(scope)
        run((a) => a.stage([png(`${scope}.png`)]))
      }

      expect(revoke).toHaveBeenCalledWith(url)
      setScope('room-a')
      expect(api.current!.batch.items).toEqual([])
      // The three most recent kept theirs.
      setScope('room-b')
      expect(api.current!.batch.items).toHaveLength(1)
      revoke.mockRestore()
    })

    it('applies a downscale that lands after the room changed', async () => {
      // `shrink` resolves its item by id across every scope. Mapping over the
      // *active* bucket would drop the result and leak the full-size url it was
      // replacing — for the one case nobody would think to look at.
      const revoke = vi.spyOn(URL, 'revokeObjectURL')
      let settle: (url: string | null) => void = () => {}
      downscale.mockImplementationOnce(
        () => new Promise<string | null>((resolve) => (settle = resolve)),
      )
      const { api, run, setScope } = harness('room-a')
      run((a) => a.stage([png('a.png')]))
      const original = api.current!.batch.items[0].previewUrl!

      setScope('room-b')
      await act(async () => {
        settle('blob:small')
      })

      // The full-size url it replaced is gone, not leaked.
      expect(revoke).toHaveBeenCalledWith(original)
      setScope('room-a')
      expect(api.current!.batch.items[0].previewUrl).toBe('blob:small')
      revoke.mockRestore()
    })

    it('discards a downscale whose file was retired while it decoded', async () => {
      const revoke = vi.spyOn(URL, 'revokeObjectURL')
      let settle: (url: string | null) => void = () => {}
      downscale.mockImplementationOnce(
        () => new Promise<string | null>((resolve) => (settle = resolve)),
      )
      const { api, run } = harness('room-a')
      run((a) => a.stage([png('a.png')]))
      const id = api.current!.batch.items[0].id

      run((a) => a.remove(id))
      await act(async () => {
        settle('blob:small')
      })

      // Not resurrected, and the replacement is not leaked either.
      expect(api.current!.batch.items).toEqual([])
      expect(revoke).toHaveBeenCalledWith('blob:small')
      revoke.mockRestore()
    })

    it("revokes every room's urls on sign-out, not just the open one", () => {
      const revoke = vi.spyOn(URL, 'revokeObjectURL')
      const { api, run, setScope, staging } = harness('room-a')
      run((a) => a.stage([png('a.png')]))
      const first = api.current!.batch.items[0].previewUrl!
      setScope('room-b')
      run((a) => a.stage([png('b.png')]))
      const second = api.current!.batch.items[0].previewUrl!

      staging.clearAll()

      expect(revoke).toHaveBeenCalledWith(first)
      expect(revoke).toHaveBeenCalledWith(second)
      revoke.mockRestore()
    })

    it('survives an unmount and remount of the room view', () => {
      // The mobile route in miniature: room -> list (unmount) -> room.
      const staging = createAttachmentStaging()
      const first = harness('room-a', staging)
      first.run((a) => a.stage([png('a.png')]))
      const url = first.api.current!.batch.items[0].previewUrl!
      first.unmount()

      const second = harness('room-a', staging)

      expect(second.api.current!.batch.items.map((i) => i.file.name)).toEqual([
        'a.png',
      ])
      expect(second.api.current!.batch.items[0].previewUrl).toBe(url)
    })

    it('does not bump the revision for a visit that evicts nothing', () => {
      // `touch` bumps only when it actually retired a scope, because the bump
      // is what re-renders every composer subscribed to staging. Entering a
      // room reorders the LRU and changes nothing anyone can see, so bumping
      // unconditionally — the obvious way to make an eviction safe — would
      // re-render every composer on every room entry instead.
      const staging = createAttachmentStaging()
      staging.stage('room-a', [png('a.png')])
      const before = staging.revision.value

      staging.touch('room-b')
      staging.touch('room-a')

      expect(staging.revision.value).toBe(before)
      expect(staging.batch('room-a').items).toHaveLength(1)
    })
  })
})
