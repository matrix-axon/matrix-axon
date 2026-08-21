import { signal, type ReadonlySignal } from '@preact/signals'
import { MAX_UPLOAD_BYTES, uploadKind } from './media-service'
import { downscalePreview } from './downscale-preview'

/** One file staged for sending, with its local preview (images only). */
export interface StagedAttachment {
  /**
   * A stable identity for keying and removal. A `File` is not usable as a
   * key — two identical pastes are equal enough to collide, and Preact would
   * pair the wrong chip with the wrong file (WCR-01).
   */
  id: string
  file: File
  previewUrl: string | null
}

export interface AttachmentBatch {
  items: readonly StagedAttachment[]
  /** Files the last `stage` refused, and why — surfaced by the composer. */
  skipped: number
  skippedReason: 'count' | 'size' | null
  totalBytes: number
}

/** Beyond this many, the strip stops being scannable and the wait is long. */
export const MAX_BATCH_FILES = 10

/**
 * How many rooms (or threads) keep their staged files while you are elsewhere.
 *
 * Deliberately much smaller than the timeline store's cap (ADR 0085 phase 1):
 * a cached slice holds ~1 KB events, while a bucket here holds real file
 * bytes — up to `MAX_UPLOAD_BYTES` of them — so this cap is load-bearing
 * rather than a guard against staleness. Three covers the case that prompted
 * it (stage a file, glance at another room, come back).
 */
export const MAX_RETAINED_SCOPES = 3

const EMPTY: AttachmentBatch = {
  items: [],
  skipped: 0,
  skippedReason: null,
  totalBytes: 0,
}

/**
 * Files staged for sending but not yet sent, per room or thread (issue #89).
 *
 * **This lives in the service graph, not in a component**, and that is the
 * whole point. `RoomPage` unmounts whenever the route leaves a room — which on
 * a phone is *every* room change, since the trip is room → `/` → room with no
 * sidebar to click. A first version held these buckets in `useAttachments`
 * itself; it survived the desktop room-to-room switch and died on the mobile
 * path, which is precisely the platform the report came from. The timeline
 * store cache (ADR 0085 phase 1) already had to live out here for the same
 * reason.
 *
 * `scope` is the room or thread the files belong to (`accountId\0roomId`, plus
 * the thread root in `ThreadPanel`). Nothing else keeps a file staged in room A
 * from being sent in room B, so every operation takes it explicitly.
 *
 * Staging is **additive**: pick three, paste a fourth, remove one. Every
 * preview object url is owned here and revoked on remove, clear, scope
 * eviction, and `clearAll`. That bookkeeping is the most bug-prone part of
 * this file — a leaked object url is invisible until someone reads a memory
 * profile — which is why the urls live in one map keyed by id (a uuid, so ids
 * never collide across scopes) rather than being derived from the buckets.
 */
export interface AttachmentStaging {
  /** Bumped by every mutation; read it to subscribe a component. */
  revision: ReadonlySignal<number>
  /** What is staged for a scope. Empty — never undefined — when nothing is. */
  batch(scope: string): AttachmentBatch
  /** Add files to a scope, applying the count and byte caps. */
  stage(scope: string, files: FileList | readonly File[]): void
  remove(scope: string, id: string): void
  /** Empty one scope — what the send path calls once the files are claimed. */
  clear(scope: string): void
  /** Mark a scope most recently used, and retire the oldest past the cap. */
  touch(scope: string): void
  /** Drop everything, revoking every url. Sign-out only. */
  clearAll(): void
}

interface Bucket {
  items: readonly StagedAttachment[]
  skipped: number
  skippedReason: 'count' | 'size' | null
}

export function createAttachmentStaging(
  limit: number = MAX_RETAINED_SCOPES,
): AttachmentStaging {
  /** Insertion-ordered: the front is the least recently active scope. */
  const buckets = new Map<string, Bucket>()
  const urls = new Map<string, string>()
  const revision = signal(0)

  const changed = () => {
    revision.value += 1
  }

  function revoke(id: string): void {
    const url = urls.get(id)
    if (url !== undefined) {
      URL.revokeObjectURL(url)
      urls.delete(id)
    }
  }

  function drop(scope: string): void {
    const held = buckets.get(scope)
    if (held === undefined) {
      return
    }
    for (const item of held.items) {
      revoke(item.id)
    }
    buckets.delete(scope)
  }

  /**
   * Retire the least recently active scopes past the cap; never the active one.
   *
   * Reports whether it actually dropped anything, because dropping revokes
   * object urls: a caller that does not follow this with `changed()` leaves
   * any component still rendering that scope pointing at revoked blobs, with
   * no signal telling it to re-read. `stage()` bumps unconditionally, so only
   * `touch()` has to consult the result.
   */
  function retire(active: string): boolean {
    let dropped = false
    for (const key of [...buckets.keys()]) {
      if (buckets.size <= limit) {
        break
      }
      if (key !== active) {
        drop(key)
        dropped = true
      }
    }
    return dropped
  }

  /**
   * Swap an item's preview for a downscaled one, and retire the original.
   *
   * Staging shows the full-size object url immediately — ten files should
   * appear at once, not after ten decodes — and each is replaced as its
   * smaller version is ready.
   *
   * The item is found **by id across every scope**, because the decode can
   * outlive the room the file was staged in. Resolving against the active
   * scope instead would silently drop the result *and* leak the full-size url
   * it was replacing, for the one case (change rooms mid-decode) nobody would
   * think to look at.
   */
  async function shrink(id: string, file: File): Promise<void> {
    const smaller = await downscalePreview(file)
    if (smaller === null) {
      return
    }
    const holder = [...buckets].find(([, held]) =>
      held.items.some((item) => item.id === id),
    )
    const original = urls.get(id)
    if (holder === undefined || original === undefined) {
      // Removed, cleared, or evicted while we were decoding — do not resurrect
      // it, and do not leak the replacement either.
      URL.revokeObjectURL(smaller)
      return
    }
    urls.set(id, smaller)
    URL.revokeObjectURL(original)
    const [key, held] = holder
    buckets.set(key, {
      ...held,
      items: held.items.map((item) =>
        item.id === id ? { ...item, previewUrl: smaller } : item,
      ),
    })
    changed()
  }

  return {
    revision,

    batch(scope) {
      const held = buckets.get(scope)
      if (held === undefined) {
        return EMPTY
      }
      return {
        items: held.items,
        skipped: held.skipped,
        skippedReason: held.skippedReason,
        totalBytes: held.items.reduce((sum, item) => sum + item.file.size, 0),
      }
    },

    stage(scope, files) {
      const incoming = [...files]
      if (incoming.length === 0) {
        return
      }
      const current = buckets.get(scope) ?? { ...EMPTY, items: [] }
      const accepted: StagedAttachment[] = []
      let refused = 0
      let reason: 'count' | 'size' | null = null
      let bytes = current.items.reduce((sum, item) => sum + item.file.size, 0)

      for (const file of incoming) {
        if (current.items.length + accepted.length >= MAX_BATCH_FILES) {
          refused += 1
          reason = 'count'
          continue
        }
        // Against the *accumulated* total, not per file: ten files under the
        // cap individually can be far over it together.
        if (bytes + file.size > MAX_UPLOAD_BYTES) {
          refused += 1
          reason ??= 'size'
          continue
        }
        bytes += file.size
        const id = crypto.randomUUID()
        const url =
          uploadKind(file) === 'image' ? URL.createObjectURL(file) : null
        if (url !== null) {
          urls.set(id, url)
        }
        accepted.push({ id, file, previewUrl: url })
      }

      buckets.set(scope, {
        items: [...current.items, ...accepted],
        skipped: refused,
        skippedReason: reason,
      })
      // Staging is the other way the map can pass the cap, besides a visit.
      retire(scope)
      for (const item of accepted) {
        if (item.previewUrl !== null) {
          void shrink(item.id, item.file)
        }
      }
      changed()
    },

    remove(scope, id) {
      const held = buckets.get(scope)
      if (held === undefined) {
        return
      }
      revoke(id)
      buckets.set(scope, {
        ...held,
        items: held.items.filter((item) => item.id !== id),
      })
      changed()
    },

    clear(scope) {
      if (buckets.has(scope)) {
        drop(scope)
        changed()
      }
    },

    touch(scope) {
      const held = buckets.get(scope)
      if (held !== undefined) {
        buckets.delete(scope)
        buckets.set(scope, held)
      }
      // Reordering alone is invisible, so it must not bump — every composer
      // would re-render on every room entry. An eviction is not invisible: it
      // revokes another scope's object urls, and a surface still showing that
      // scope (a `ThreadPanel` open beside the room it belongs to, each
      // touching its own) would go on painting revoked blobs until some
      // unrelated mutation happened to refresh it. Today `stage()` re-trims to
      // the cap on every insertion, so this branch is unreachable; that is an
      // invariant of a different function, and this is the cheap way not to
      // depend on it.
      if (retire(scope)) {
        changed()
      }
    },

    clearAll() {
      // Nothing staged means nothing to announce. Not an optimisation: the
      // sign-out effect that calls this runs inside a signal flush, and a bump
      // that fires even when the wipe found nothing re-triggers that effect,
      // which wipes and bumps again — a flush that never reaches a fixed
      // point, which @preact/signals aborts after 100 iterations with "Cycle
      // detected". Any wipe reachable from an effect has to be idempotent.
      if (buckets.size === 0 && urls.size === 0) {
        return
      }
      for (const key of [...buckets.keys()]) {
        drop(key)
      }
      // Belt and braces: an id whose bucket is already gone would otherwise
      // keep its url alive for the life of the tab.
      for (const url of urls.values()) {
        URL.revokeObjectURL(url)
      }
      urls.clear()
      changed()
    },
  }
}
