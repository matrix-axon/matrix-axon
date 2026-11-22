/**
 * What a platform's `mediaStreamSrc` throws when the platform cannot stream
 * after all, such as when the Linux shell's loopback server cannot start.
 * `issue` (`stream-ticket.ts`) then answers `refused`, so the caller falls
 * back to the blob path.
 *
 * A module of its own, importing nothing, because `platform/media-stream.ts`
 * needs the class at runtime: importing it from `stream-ticket.ts`, which
 * imports from `platform`, would leave it undefined at load the day that
 * import stops being type-only.
 */
export class StreamUnavailableError extends Error {
  override name = 'StreamUnavailableError'
}
