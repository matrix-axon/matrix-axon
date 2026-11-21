import { convertFileSrc, invoke } from '@tauri-apps/api/core'
import { StreamUnavailableError } from '../media/stream-ticket'

/**
 * The page's side of the shell's stream-ticket proxy (ADR 0139 § 7), served
 * by the in-repo `src-tauri/axon-http` plugin.
 *
 * A `<video>` in the shell cannot load the server's ticket URL itself: the
 * CSP cannot list a self-hosted server in `media-src`, and a plain-http server
 * is mixed content from the custom-scheme page. So the plugin proxies
 * `<base>/v1/media-stream/<ticket>` on the shell's pooled client, and the
 * element loads it from the shell:
 *
 * - Through the `axon-media` scheme ({@link shellMediaStreamSrc}):
 *   `axon-media://localhost/<ticket>`, or `http://axon-media.localhost/<ticket>`
 *   on Windows and Android (`convertFileSrc` picks the form).
 * - On Linux, from a loopback HTTP server ({@link loopbackMediaStreamSrc}):
 *   `http://127.0.0.1:<port>/<token>/<ticket>`. WebKitGTK's GStreamer player
 *   admits only its built-in URL schemes, and GStreamer has no source for a
 *   custom one, so it cannot play the scheme
 *   ([issue 1192](https://github.com/matrix-axon/matrix-axon/issues/1192)).
 *
 * The plugin holds the base, not the URL, so the page cannot make either
 * reach anything but the ticket route. The base is sent whenever it differs
 * from the last one sent, before the element is given its `src`. Sends run one
 * at a time, so `sentBase` always names the base the plugin last accepted.
 *
 * The plugin holds one base, so two servers' videos cannot play at once; the
 * later one's base wins.
 */

/** Where every ticket path the server issues begins. */
const STREAM_PREFIX = '/v1/media-stream/'

/** The scheme the plugin registers (`media::SCHEME`). */
const SCHEME = 'axon-media'

/** The base the plugin was last given, by this page; `null` if unknown. */
let sentBase: string | null = null

/** The latest base send, which the next one waits for. Never rejects. */
let sending: Promise<void> = Promise.resolve()

/**
 * Send the base of a ticket URL (`<server base>/v1/media-stream/<ticket>`)
 * to the plugin if it differs from the last one sent, and return the ticket.
 * Rejects if the URL is not a ticket URL or the plugin refuses the base,
 * which `issue` reports as a failure.
 */
async function sendBase(ticketUrl: string): Promise<string> {
  const at = ticketUrl.lastIndexOf(STREAM_PREFIX)
  const ticket = at < 0 ? '' : ticketUrl.slice(at + STREAM_PREFIX.length)
  if (ticket === '' || ticket.includes('/')) {
    throw new Error('not a stream ticket URL')
  }
  const base = ticketUrl.slice(0, at)
  const send = sending.then(async () => {
    if (base === sentBase) return
    // Unknown until the plugin answers: a refusal leaves it unknown, so the
    // next call sends again.
    sentBase = null
    await invoke('plugin:axon-http|set_media_stream_base', { base })
    sentBase = base
  })
  sending = send.catch(() => {})
  await send
  return ticket
}

/** Map a ticket URL to the `axon-media` scheme URL a media element loads. */
export async function shellMediaStreamSrc(ticketUrl: string): Promise<string> {
  return convertFileSrc(await sendBase(ticketUrl), SCHEME)
}

/**
 * The loopback server's `http://127.0.0.1:<port>/<token>/`, asked for once
 * per page; `null` before that, or after it failed so the next call asks
 * again.
 */
let loopbackPrefix: Promise<string> | null = null

/**
 * Map a ticket URL to the loopback URL a media element in the Linux shell
 * loads. The plugin starts its loopback server on the first call. If it
 * cannot, this rejects with {@link StreamUnavailableError}, and `issue`
 * sends the caller to the blob path.
 */
export async function loopbackMediaStreamSrc(
  ticketUrl: string,
): Promise<string> {
  const ticket = await sendBase(ticketUrl)
  const asked = (loopbackPrefix ??= invoke<string>(
    'plugin:axon-http|media_loopback',
  ))
  try {
    return `${await asked}${ticket}`
  } catch {
    if (loopbackPrefix === asked) loopbackPrefix = null
    throw new StreamUnavailableError('the loopback media server is not running')
  }
}

/**
 * The shell's `mediaStreamSrc` for one build target (`TAURI_ENV_PLATFORM`):
 * the loopback server on Linux, the `axon-media` scheme everywhere else.
 *
 * Android streams through the scheme since wry 0.57.1. Android WebView
 * applies a request's `Range` to an intercepted response itself, which broke
 * every range after byte 0 until wry offset the skip
 * ([issue 1194](https://github.com/matrix-axon/matrix-axon/issues/1194)).
 */
export function mediaStreamSrcFor(
  target: string,
): (ticketUrl: string) => Promise<string> {
  return target === 'linux' ? loopbackMediaStreamSrc : shellMediaStreamSrc
}

/** Forget the base sent, as a fresh page would. Tests only. */
export function resetMediaStreamBaseForTests(): void {
  sentBase = null
  sending = Promise.resolve()
  loopbackPrefix = null
}
