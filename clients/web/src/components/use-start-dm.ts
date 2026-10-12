import { useCallback, useEffect, useRef, useState } from 'preact/hooks'
import { useServices } from '../services'

/**
 * Start a direct message, shared by Room information and User information.
 *
 * The result is dropped when the caller invalidates (the panel moved to
 * another user or room) or unmounts, so a slow `createDm` cannot route away
 * from whatever the user is looking at now. A thrown `createDm` is the same
 * failure as `{ ok: false }`. An older start must not clear a newer one's
 * pending state.
 */
export function useStartDm(
  accountId: string,
  onStarted: (roomId: string) => void,
): {
  startDm: (userId: string) => Promise<void>
  pendingUserId: string | null
  error: string | null
  invalidate: () => void
} {
  const { rooms } = useServices()
  const [pendingUserId, setPendingUserId] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const generation = useRef(0)
  const pendingRef = useRef<string | null>(null)
  const onStartedRef = useRef(onStarted)
  useEffect(() => {
    onStartedRef.current = onStarted
  })

  useEffect(
    () => () => {
      generation.current += 1
    },
    [],
  )

  const invalidate = useCallback(() => {
    generation.current += 1
    pendingRef.current = null
    setPendingUserId(null)
    setError(null)
  }, [])

  const startDm = async (userId: string) => {
    if (pendingRef.current !== null) {
      return
    }
    const request = ++generation.current
    pendingRef.current = userId
    setError(null)
    setPendingUserId(userId)
    try {
      const result = await rooms.createDm(accountId, userId)
      if (request !== generation.current) {
        return
      }
      if (!result.ok) {
        setError(`Could not start DM with ${userId}: ${result.message}`)
        return
      }
      onStartedRef.current(result.roomId)
    } catch (cause) {
      if (request !== generation.current) {
        return
      }
      const message = cause instanceof Error ? cause.message : String(cause)
      setError(`Could not start DM with ${userId}: ${message}`)
    } finally {
      if (request === generation.current) {
        pendingRef.current = null
        setPendingUserId(null)
      }
    }
  }

  return { startDm, pendingUserId, error, invalidate }
}
