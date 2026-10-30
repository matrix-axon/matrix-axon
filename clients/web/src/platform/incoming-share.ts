import { invoke } from '@tauri-apps/api/core'
import type { IncomingSharePort } from '../incoming-share/types'
import { isIncomingShare } from '../incoming-share/types'

export function nativeIncomingShare(): IncomingSharePort {
  return {
    async call<T>(
      action: string,
      args: Record<string, unknown> = {},
    ): Promise<T> {
      const result = await invoke<{ value: T }>('plugin:native-auth|share', {
        request: { action, ...args },
      })
      if (
        action === 'list' &&
        (!Array.isArray(result.value) ||
          result.value.length > 20 ||
          !result.value.every(isIncomingShare))
      )
        throw new Error('Incoming share: invalid')
      if (
        ['claim', 'update', 'retain', 'seed'].includes(action) &&
        !isIncomingShare(result.value)
      )
        throw new Error('Incoming share: invalid')
      return result.value
    },
    subscribe(listener) {
      const visible = () => {
        if (!document.hidden) listener()
      }
      window.addEventListener('focus', listener)
      document.addEventListener('visibilitychange', visible)
      return () => {
        window.removeEventListener('focus', listener)
        document.removeEventListener('visibilitychange', visible)
      }
    },
  }
}
