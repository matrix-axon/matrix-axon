import { useEffect, useRef } from 'preact/hooks'
import PRIVACY_POLICY_HTML from 'virtual:privacy-policy'

/**
 * App-wide event asking whatever screen is up to show the privacy policy. The
 * macOS Help menu raises it (via `AppRoot`), and it has to work on every
 * screen, including the two that come before the router exists.
 */
export const SHOW_PRIVACY_EVENT = 'axon:show-privacy'

/** Run `handler` whenever something asks for the privacy policy. */
export function useShowPrivacyRequest(handler: () => void): void {
  const latest = useRef(handler)
  latest.current = handler
  useEffect(() => {
    const onShow = () => latest.current()
    window.addEventListener(SHOW_PRIVACY_EVENT, onShow)
    return () => window.removeEventListener(SHOW_PRIVACY_EVENT, onShow)
  }, [])
}

/**
 * The privacy policy, bundled from `docs/PRIVACY_POLICY.md` at build time (see
 * the `axon-privacy-policy` plugin in vite.config.ts). App stores require the
 * policy to be reachable from inside the app, and a bundled copy still opens
 * when the server cannot be reached.
 *
 * Signed in, it is the `/privacy` route, linked from the Settings footer and
 * the help dialog. The server-setup and sign-in screens have no router, so
 * they show it in place and pass `onBack`. Either way it carries its own way
 * back, since a standalone PWA or the shell may have no back button.
 */
export function PrivacyPage({ onBack }: { onBack?: () => void }) {
  return (
    <div class="page privacy-policy">
      {/* Trusted: our own Markdown, rendered at build time, not user content. */}
      <div dangerouslySetInnerHTML={{ __html: PRIVACY_POLICY_HTML }} />
      <p>
        {onBack === undefined ? (
          <a href="/settings">← Back to settings</a>
        ) : (
          <button type="button" class="link-button" onClick={onBack}>
            ← Back
          </button>
        )}
      </p>
    </div>
  )
}

/**
 * The policy link on the screens before sign-in, and the in-place view it
 * opens. Returns the policy page while it is open, else `null` and a link.
 */
export function usePrivacyInPlace(
  open: boolean,
  setOpen: (open: boolean) => void,
) {
  useShowPrivacyRequest(() => setOpen(true))
  return {
    page: open ? <PrivacyPage onBack={() => setOpen(false)} /> : null,
    link: (
      <p class="signin-privacy muted">
        <button type="button" class="link-button" onClick={() => setOpen(true)}>
          Privacy policy
        </button>
      </p>
    ),
  }
}
