import { useEffect } from 'preact/hooks'
import { parseMatrixRoomReference } from './matrix-to'
import type { Platform } from './platform'

/**
 * Whether this href leaves the app.
 *
 * Same-origin links are the client's own routes and must stay in-window; a
 * `matrix:` link is handled by the caller before this is reached. Anything
 * http(s) elsewhere is a link to the web, which in a packaged build has to be
 * handed to the user's real browser.
 */
export function isExternalHref(href: string): boolean {
  let url: URL
  try {
    url = new URL(href, window.location.href)
  } catch {
    return false
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    return false
  }
  return url.origin !== window.location.origin
}

/**
 * Hand every external link on the page to the platform's opener.
 *
 * In a packaged build an untouched external link either navigates the *app
 * window* to that page, with no back button to return by, or, given
 * `target="_blank"`, asks for a new window that the shell's webview silently
 * drops. Either way the link appears to do nothing or loses the app. Message
 * bodies and the bundled privacy policy both render arbitrary links, so this
 * is caught centrally rather than per-component.
 *
 * Mounted on *every* screen: by `App`, which covers the signed-in shell and
 * the sign-in screen, and by `ServerSetup`, which `AppRoot` shows before `App`
 * exists. It used to live in the signed-in shell alone, which left the privacy
 * policy's contact link dead on the two screens an App Store reviewer sees
 * first.
 *
 * Inert in a browser, where `openExternal` is null because the anchor's own
 * behaviour is right and modified or middle clicks must keep opening tabs.
 * In the shell there are no tabs, so every click, modified or middle, is
 * answered here. A right click (`auxclick`, button 2) is a context menu, not
 * a request to open anything.
 *
 * Matrix room links are left to the signed-in shell, which joins them in the
 * active account instead of leaving the app.
 */
export function useExternalLinks(
  platform: Partial<Pick<Platform, 'openExternal'>>,
): void {
  const openExternal = platform.openExternal ?? null
  useEffect(() => {
    if (openExternal === null) {
      return
    }
    const onClick = (event: MouseEvent) => {
      if (event.defaultPrevented) {
        return
      }
      if (event.type === 'auxclick' && event.button !== 1) {
        return
      }
      const anchor = (event.target as Element | null)?.closest?.('a[href]')
      if (!(anchor instanceof HTMLAnchorElement) || anchor.download !== '') {
        return
      }
      if (
        parseMatrixRoomReference(anchor.href) !== null ||
        !isExternalHref(anchor.href)
      ) {
        return
      }
      event.preventDefault()
      // Logged, not swallowed. There is nothing to show the user — the click
      // is already prevented, so no fallback remains — but a denied capability
      // scope or an absent handler presents exactly as a link that does
      // nothing, and that needs a trace somewhere. `openExternal` builds this
      // message to be safe to record: an origin, never the query, which can
      // carry a signed media URL or credentials.
      void openExternal(anchor.href).catch((error: unknown) => {
        console.error(error)
      })
    }
    // A middle click raises `auxclick`, not `click`, in every engine this
    // runs on.
    document.addEventListener('click', onClick)
    document.addEventListener('auxclick', onClick)
    return () => {
      document.removeEventListener('click', onClick)
      document.removeEventListener('auxclick', onClick)
    }
  }, [openExternal])
}
