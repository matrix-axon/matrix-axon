/** SHA-256 of the room id, first 32 hex characters. Matches the remote collapse id. */
const COLLAPSE_ID = /^[0-9a-f]{32}$/

/**
 * The value a local notification must share with the remote alert (ADR 0113).
 *
 * Apple uses it as the notification identifier (`apns-collapse-id`, at most
 * 64 bytes). Android uses it as the notification tag. A value that is not
 * 32 hex characters is refused so a bad digest cannot be posted.
 */
export async function notificationCollapseId(
  roomId: string,
): Promise<string | null> {
  const digest = await crypto.subtle.digest(
    'SHA-256',
    new TextEncoder().encode(roomId),
  )
  const hex = [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, '0'))
    .join('')
  const collapseId = hex.slice(0, 32)
  return COLLAPSE_ID.test(collapseId) ? collapseId : null
}
