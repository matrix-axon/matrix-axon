import { convertFileSrc, invoke } from '@tauri-apps/api/core'

/**
 * The page's side of the shell's `axon-media` scheme (ADR 0139 § 7), served
 * by the in-repo `src-tauri/axon-http` plugin.
 *
 * A `<video>` in the shell cannot load the server's ticket URL itself: the
 * CSP cannot list a self-hosted server in `media-src`, and a plain-http server
 * is mixed content from the custom-scheme page. So the element loads
 * `axon-media://localhost/<ticket>` (`http://axon-media.localhost/<ticket>` on
 * Windows and Android; `convertFileSrc` picks the form), and the plugin
 * requests `<base>/v1/media-stream/<ticket>` on the shell's pooled client.
 *
 * The plugin holds the base, not the URL, so the page cannot make the scheme
 * reach anything but the ticket route. The base is sent whenever it differs
 * from the last one sent, before the element is given its `src`.
 */

/** Where every ticket path the server issues begins. */
const STREAM_PREFIX = '/v1/media-stream/'

/** The scheme the plugin registers (`media::SCHEME`). */
const SCHEME = 'axon-media'

/** The base the plugin was last given, by this page. */
let sentBase: string | null = null

/**
 * Map a ticket URL (`<server base>/v1/media-stream/<ticket>`) to the scheme
 * URL a media element in the shell loads. Rejects if the URL is not a ticket
 * URL or the plugin refuses the base, which `issue` reports as a failure.
 */
export async function shellMediaStreamSrc(ticketUrl: string): Promise<string> {
  const at = ticketUrl.lastIndexOf(STREAM_PREFIX)
  const ticket = at < 0 ? '' : ticketUrl.slice(at + STREAM_PREFIX.length)
  if (ticket === '' || ticket.includes('/')) {
    throw new Error('not a stream ticket URL')
  }
  const base = ticketUrl.slice(0, at)
  if (base !== sentBase) {
    await invoke('plugin:axon-http|set_media_stream_base', { base })
    sentBase = base
  }
  return convertFileSrc(ticket, SCHEME)
}

/**
 * The build targets (`TAURI_ENV_PLATFORM`) whose webview player cannot load
 * the scheme, so video and audio there keep the blob path (ADR 0139 § 7).
 *
 * - `linux`: WebKitGTK's GStreamer player admits only its built-in URL
 *   schemes, and GStreamer has no source for a custom one, so every media
 *   element fails before it sends a request
 *   ([issue 1192](https://github.com/matrix-axon/matrix-axon/issues/1192)).
 * - `android` and `androideabi`: Android WebView applies a request's `Range`
 *   to an intercepted response itself, skipping into a body that holds only
 *   the requested bytes, so every range after byte 0 fails
 *   ([issue 1194](https://github.com/matrix-axon/matrix-axon/issues/1194)).
 *
 * The page's own `fetch` of the scheme works on Linux, and from byte 0 on
 * Android, which is not enough for a player.
 */
const NO_STREAMING: ReadonlySet<string> = new Set([
  'linux',
  'android',
  'androideabi',
])

/**
 * The shell's `mediaStreamSrc` for one build target, or `null` where the
 * webview's player cannot load the scheme ({@link NO_STREAMING}).
 */
export function mediaStreamSrcFor(
  target: string,
): ((ticketUrl: string) => Promise<string>) | null {
  return NO_STREAMING.has(target) ? null : shellMediaStreamSrc
}

/** Forget the base sent, as a fresh page would. Tests only. */
export function resetMediaStreamBaseForTests(): void {
  sentBase = null
}
