/**
 * A random opaque identifier, on every origin the client can be served from.
 *
 * `crypto.randomUUID` is **secure-context only**. It is absent over plain http,
 * which is how this client is reached during LAN development — and the failure
 * is not a degraded feature but a hard crash: `loadDeviceId`
 * (`stores/device-state.ts`) mints an id while `createServices()` builds the
 * graph, inside the first render, so the `TypeError` escapes before anything
 * paints and the app is a blank page with one console line. A new origin is
 * exactly when it happens, too, because `localStorage` is per-origin: the id
 * that makes a previously-visited https origin work is one this origin has
 * never stored.
 *
 * `crypto.getRandomValues` is *not* secure-context gated, so the fallback is a
 * real version 4 UUID rather than a weaker shape — same bytes, assembled here.
 * The last resort covers a `crypto` that is missing entirely, which no target
 * browser has; it exists so this function cannot be the thing that throws.
 *
 * Every caller wants a **discriminator, not a secret**: a device id, a
 * `local:` echo id, a staged-attachment id. None of them authenticates
 * anything or is sent anywhere as a credential, which is what makes a
 * non-cryptographic last resort acceptable — the same reasoning
 * `cacheNamespace` (`stores/cache-store.ts`) already applies to its digest.
 * Anything that *is* a secret must not come from here.
 */
export function randomId(): string {
  const webCrypto = globalThis.crypto as Crypto | undefined
  if (typeof webCrypto?.randomUUID === 'function') {
    return webCrypto.randomUUID()
  }
  const bytes = new Uint8Array(16)
  if (typeof webCrypto?.getRandomValues === 'function') {
    webCrypto.getRandomValues(bytes)
  } else {
    for (let i = 0; i < bytes.length; i += 1) {
      bytes[i] = Math.floor(Math.random() * 256)
    }
  }
  // Version 4, variant 10 — the two fields that make the bytes a valid UUID.
  bytes[6] = (bytes[6]! & 0x0f) | 0x40
  bytes[8] = (bytes[8]! & 0x3f) | 0x80
  const hex = [...bytes].map((byte) => byte.toString(16).padStart(2, '0'))
  return [
    hex.slice(0, 4).join(''),
    hex.slice(4, 6).join(''),
    hex.slice(6, 8).join(''),
    hex.slice(8, 10).join(''),
    hex.slice(10, 16).join(''),
  ].join('-')
}
