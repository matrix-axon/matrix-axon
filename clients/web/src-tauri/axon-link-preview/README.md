# axon-link-preview

The shell's link-preview fetcher (ADR 0132 § 3). The composer asks it, through
`Platform.linkPreview(url, signal)` in `src/platform/link-preview.ts`, for the
title, description, site name and image of a link the user wrote, so the
preview can be sent with the message instead of built later by each
recipient.

It is a separate plugin from `axon-http` so that it neither uses that
transport's any-URL reach nor stands in the way of narrowing it (issue 893).

## The boundary

The same one as the Share Extension's Swift reader
(`incoming-share/Sources/AxonIncomingShare/PreviewNetwork.swift`), rule for rule:

| Rule        | Where                                                                                                                                                                               |
| ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Hosts       | `url_check.rs`: HTTPS, a public-looking DNS name, no IP literal, credentials, `xn--` label or reserved suffix                                                                       |
| DNS         | `dns/`: the origin's A records only; NAT64 through `ipv4only.arpa` (RFC 7050)                                                                                                       |
| Addresses   | `address.rs`: a public IPv4 address, or an IPv6 address synthesized here from one                                                                                                   |
| Connection  | `net.rs`: to the validated address only, no proxy; TLS 1.2+ against the original name, Mozilla's bundled roots                                                                      |
| Request     | `net.rs`: a plain `GET`, no cookies or credentials, `Accept-Encoding: identity`                                                                                                     |
| Redirects   | `net.rs`: by hand, six requests at most, each validated as a new one                                                                                                                |
| Responses   | `http.rs`: status 200 only; 32 KiB of headers; a page until `</head` or `<body` followed by a tag-name boundary, or 256 KiB; an image up to 2 MiB                                   |
| Parsing     | `page.rs`: only the bytes before a head's 513th `<`; the Swift reader's fields, selectors and trimming                                                                              |
| Image       | `image.rs`: PNG, JPEG, GIF or WebP; at most 1200 px; re-encoded as JPEG, or PNG when transparent; no metadata kept                                                                  |
| Time        | `preview.rs`: one 25-second deadline for the page, its redirects and its image; cancellable throughout                                                                              |
| Concurrency | `lib.rs`: two lookups at once across the app, 16 registered; one 25 s deadline from before the wait for a slot; decodes and Windows DNS calls have small bounded pools of their own |

`src/incoming-share/fixtures/preview-boundary.json` holds the cases both readers
must agree on: URLs, address scope, operator NAT64 mappings, RFC 6052 vectors,
resolution outcomes and page metadata. `src/tests.rs` and the Swift package's
`BoundaryFixtureTests.swift` both read it, and `device-preview-urls.json` beside
it, which the composer's TypeScript check reads too. Several of the metadata
cases were settled by measuring the Swift reader, not by reading its source:
Foundation's trim includes U+200B, and SwiftSoup collapses neither a no-break
space nor a vertical tab inside a title.

## Resolving without ever asking for the origin's AAAA records

ADR 0132 requires that the origin is only ever asked for A records, and
`ipv4only.arpa` only for AAAA, on all five platforms, proved rather than assumed
from a family hint. Rust's `ToSocketAddrs` cannot promise that, and neither can
`getaddrinfo` with `AF_INET`, which is a hint. So each platform is asked for a
record _type_:

| Platform    | Call                                                                    | Applies                                                      |
| ----------- | ----------------------------------------------------------------------- | ------------------------------------------------------------ |
| macOS, iOS  | `DNSServiceGetAddrInfo`, `kDNSServiceProtocol_IPv4` / `_IPv6`           | VPN, scoped and encrypted DNS; same as the Swift reader      |
| Android 10+ | `android_res_nquery`, `ns_t_a` / `ns_t_aaaa`                            | the network's resolver and Private DNS                       |
| Windows     | `DnsQuery_W`, `DNS_TYPE_A` / `DNS_TYPE_AAAA`                            | DNS over HTTPS and the NRPT                                  |
| Linux       | one question per query, to `/etc/resolv.conf`'s servers (`dns/wire.rs`) | whatever local resolver those name, such as systemd-resolved |

Android 7 to 9 (the shell's `minSdk` is 24) have no typed query in the NDK, so
lookups there fail as unavailable rather than fall back to a hinted one.

`hickory-resolver` was the other candidate. It sends its own queries, like
`dns/wire.rs` does, so on Android and iOS it would bypass Private DNS, VPN
resolvers and encrypted-DNS profiles, and it would have to discover nameservers
the platforms do not expose. On Linux, where `/etc/resolv.conf` is the system's
own answer, a single-question client is about 200 lines and adds no crate.

### What the tests prove, and what a capture proves

- `dns::resolve` is the only caller of a backend. With a recording backend, the
  fixture's resolution cases assert that the only questions asked are
  `(origin, A)` and `(ipv4only.arpa, AAAA)`, whatever the answers. This runs
  everywhere the tests do.
- `dns/wire.rs` is built on every platform, and is tested against a real DNS
  server on the loopback, which records each question it receives, including
  the TCP retry after a truncated answer. That is the Linux backend proved on
  the wire, on every CI and developer machine.
- The Apple, Android and Windows backends hand the platform a record type. The
  only way to see what the platform then sends is to capture it, once per
  platform, with the steps below.

### Capturing the queries

Until the composer calls the plugin (ADR 0132's web PR), ask it directly. In a
debug shell, open the webview's devtools (Ctrl+Shift+I, or the context menu's
Inspect) and run, with a new `id` each time:

```js
const frame = await window.__TAURI_INTERNALS__.invoke(
  'plugin:axon-link-preview|lookup',
  { id: 9001, url: 'https://www.wikipedia.org/' },
)
const length = new DataView(frame).getUint32(0)
console.log(new TextDecoder().decode(new Uint8Array(frame, 4, length)))
```

Capture while it runs. Expect an `A` query for the site, an `AAAA` query for
`ipv4only.arpa`, and no `AAAA` query for the site. Other parts of the app, and
the webview, make queries of their own; filter on the two names. Two things
hide queries that were made:

- **The platform's DNS cache.** A name resolved recently is answered without a
  packet. Flush it first (`ipconfig /flushdns` on Windows,
  `sudo dscacheutil -flushcache; sudo killall -HUP mDNSResponder` on macOS,
  `resolvectl flush-caches` with systemd-resolved), or use a site this machine
  has not visited.
- **Encrypted DNS** (DNS over HTTPS or TLS, Private DNS, an iCloud Private Relay
  or VPN profile): the questions are not visible as DNS. Turn it off for the
  capture.

The `device_preview_dns64` log line says what discovery found: `prefix`,
`no_record`, `unusable` or `failed`. If the `ipv4only.arpa` query does not
appear in a capture, that line tells an answer the platform gave without a
packet (`no_record` or `prefix`) from a query that failed. Either way the
lookup goes on with the IPv4 address alone, which is correct on a network
without NAT64.

## Logging

`AXON_LINK_PREVIEW_LOG=1` (always on in a debug build) writes the Swift
reader's `device_preview_*` events to stderr: kinds, hops, statuses, byte counts
and address families, never a host, URL or address.
