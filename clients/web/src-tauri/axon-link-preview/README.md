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

| Rule        | Where                                                                                                              |
| ----------- | ------------------------------------------------------------------------------------------------------------------ |
| Hosts       | `url_check.rs`: HTTPS, a public-looking DNS name, no IP literal, credentials, `xn--` label or reserved suffix      |
| DNS         | `dns/`: the origin's A records only; NAT64 through `ipv4only.arpa` (RFC 7050)                                      |
| Addresses   | `address.rs`: a public IPv4 address, or an IPv6 address synthesized here from one                                  |
| Connection  | `net.rs`: to the validated address only, no proxy; TLS 1.2+ against the original name, Mozilla's bundled roots     |
| Request     | `net.rs`: a plain `GET`, no cookies or credentials, `Accept-Encoding: identity`                                    |
| Redirects   | `net.rs`: by hand, six requests at most, each validated as a new one                                               |
| Responses   | `http.rs`: status 200 only; 32 KiB of headers; a page until its head ends or 256 KiB; an image up to 2 MiB         |
| Parsing     | `page.rs`: no page with more than 512 `<`; the Swift reader's fields, selectors and trimming                       |
| Image       | `image.rs`: PNG, JPEG, GIF or WebP; at most 1200 px; re-encoded as JPEG, or PNG when transparent; no metadata kept |
| Time        | `preview.rs`: one 25-second deadline for the page, its redirects and its image; cancellable throughout             |
| Concurrency | `lib.rs`: two lookups at once across the app; 16 registered at most                                                |

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

Build a debug shell, start a capture, type a link to a dual-stack site such as
`https://www.wikipedia.org/` into the composer, and stop the capture. Expect an
`A` query for the site and an `AAAA` query for `ipv4only.arpa`, and no `AAAA`
query for the site. Other parts of the app, and the webview, make queries of
their own; filter on the site's name.

- **macOS:** `sudo tcpdump -i any -n port 53`. With encrypted DNS configured the
  queries are not visible; turn it off for the capture.
- **iOS:** on a Mac, `rvictl -s <udid>` and `sudo tcpdump -i rvi0 -n port 53`.
- **Android:** an emulator, `adb shell` then `tcpdump -i any -n port 53` on a
  rooted image; or set Private DNS to Off and capture on the host. On
  Android 9 or earlier, expect no query at all.
- **Windows:** `pktmon start --capture --pkt-size 0 -f dns.etl`, then
  `pktmon stop` and `pktmon etl2pcap dns.etl`; or Wireshark with `dns`.
- **Linux:** `sudo tcpdump -i any -n port 53`. With systemd-resolved, also
  `resolvectl monitor`, which shows the question the stub was asked.

## Logging

`AXON_LINK_PREVIEW_LOG=1` (always on in a debug build) writes the Swift
reader's `device_preview_*` events to stderr: kinds, hops, statuses, byte counts
and address families, never a host, URL or address.
