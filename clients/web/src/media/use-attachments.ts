import { useCallback, useEffect } from 'preact/hooks'
import type { AttachmentBatch, AttachmentStaging } from './attachment-staging'

export {
  MAX_BATCH_FILES,
  MAX_RETAINED_SCOPES,
  type AttachmentBatch,
  type StagedAttachment,
} from './attachment-staging'

/**
 * The composer's view of one scope's staged attachments (ADR 0065; multi-image
 * in ADR 0081; retention in issue #89).
 *
 * The files themselves live in the service graph (`attachment-staging.ts`),
 * because `RoomPage` unmounts every time the route leaves a room — on a phone
 * that is every room change. This hook only reads the active scope's batch and
 * binds the mutations to it.
 *
 * The batch is resolved **during render** from `scope`, never swapped in by an
 * effect: an effect runs after the new room has painted, so for one frame the
 * composer would show — and `Enter` would send — the previous room's files.
 * Reading `revision` is what re-renders on a mutation.
 */
export function useAttachments(
  scope: string,
  staging: AttachmentStaging,
): {
  batch: AttachmentBatch
  stage(files: FileList | readonly File[]): void
  remove(id: string): void
  clear(): void
} {
  // Subscribe: every mutation bumps this, and the batch below is read fresh.
  void staging.revision.value
  const batch = staging.batch(scope)

  // Entering a scope makes it the most recent, which is what decides whose
  // files are retired first. Ordering only — the batch above does not wait on
  // it, so there is no frame showing the previous room's files.
  useEffect(() => {
    staging.touch(scope)
  }, [staging, scope])

  const stage = useCallback(
    (files: FileList | readonly File[]) => staging.stage(scope, files),
    [staging, scope],
  )
  const remove = useCallback(
    (id: string) => staging.remove(scope, id),
    [staging, scope],
  )
  const clear = useCallback(() => staging.clear(scope), [staging, scope])

  return { batch, stage, remove, clear }
}
