# ADR 0127 — One shared HTTP client in the native shell

**Status:** Proposed; tracked in [issue 663](https://github.com/matrix-axon/matrix-axon/issues/663).
Amends [ADR 0102](0102-tauri-packaging-and-store-distribution.md) § 2, which routes `/v1` traffic through `tauri-plugin-http`.
That ADR's authority analysis and its "target design" stand unchanged.

## Context

Every `/v1` and media request from the packaged app goes through `tauri-plugin-http` (ADR 0102 § 2).
In 2.5.9, which the shell pins, the plugin's `fetch` command builds a new `reqwest::Client` for every request (`ClientBuilder::new()` … `.build()?.request(…)` in `src/commands.rs`).
The client is dropped once its request ends, and its connection pool goes with it.
So every request pays its own DNS lookup, TCP handshake and TLS handshake, and nothing is ever multiplexed.
Upgrading would not help: the plugin's current `v2` branch still builds a client per request, and upstream issue `tauri-apps/plugins-workspace` 3024 ("plugin-http closes the connection whenever keep-alive is set") is open.

The plugin has other costs that matter to this app:

- **No connect timeout** unless the caller passes one, so a connection that never completes is bounded only by the 20-second API deadline in JavaScript.
- **Errors arrive as one flattened string** (`error sending request for url (…)`).
  The cause — DNS, refused, timed out, TLS, reset — never reaches the webview, so `api:failed` can only record `send` or `body` (see `failureKind` in `clients/web/src/api/client.ts`).
- **Request bodies cross the IPC as JSON arrays of numbers.**
  The JavaScript side does `Array.from(new Uint8Array(await req.arrayBuffer()))`.
  An upload at `MAX_UPLOAD_BYTES` (100 MiB) becomes a 100-million-element array and a JSON document several times that size.
- **Resources leak in 2.5.9.**
  The `FetchRequest` and its abort sender stay in the webview's resource table until the webview is destroyed (fixed upstream in 2.8.1).

### Field evidence, 2026-10-29

An iPhone on hotel WiFi showed waves of `api:deadline` at `stage=headers`, followed by `api:late` responses at almost exactly 35.1 s, and some at 67.1 s.
Mobile Safari on the same network, using the browser build of the same client, worked normally.

- **Axon was not slow.**
  Matching event IDs in the server log showed that axon _received_ each late request at the same moment the phone got its response, and answered in 3–8 ms.
  The 35 seconds were spent before the request reached the server.
- **35 s and 67 s are the iOS TCP SYN retransmission schedule** (cumulative 1, 2, 3, 4, 5, 7, 11, 19, 35, 67 s).
  New connections from the phone were being dropped for 20–30 s at a time, and went through on a later retry.
- **The drops were per device, not per app.**
  During an Axon stall, `curl` from a shell app on the same phone took 35.8 s to connect, all 16 parallel connections completing at the same instant.
  From a quiet start, 300 `curl` connections in waves of 20 connected in under 0.33 s each.
  So it is not a simple burst limit: something about the app's _sustained_ behaviour trips it.
- **The sustained behaviour is the connection rate.**
  The server log shows 150–430 requests a minute from the account during the bad window, and every one of them is a new TCP and TLS connection.
  Gateways that cap the connections they track per device, or rate-limit new ones over minutes, are common on hotel and guest networks.
  Safari sends the same traffic over one HTTP/2 or HTTP/3 connection and never comes near such a cap.

Which box did the dropping — the hotel gateway or a per-address limit in front of the Caddy host — was not settled, and does not change the decision.
Either way, an app that opens hundreds of connections a minute is the outlier, and other apps on the same network work because they do not.

This is not a hotel-only problem.
Earlier reports of `error sending request` on good cellular (recorded in issue 663) have the same shape.
Each handshake is a round trip or three, and on a lossy link each one is another chance to fail.

## Decision

### 1. An in-repo transport plugin holds one `reqwest::Client`

Add an in-repo Tauri plugin, `tauri-plugin-axon-http`, under `clients/web/src-tauri/axon-http/`, alongside `native-auth` and `remote-push`.
It holds one `reqwest::Client` in managed state, built once at setup and shared by every request.

The client is built with:

| Setting                                  | Value                                                 | Why                                                                                                                                                                                                          |
| ---------------------------------------- | ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| HTTP/2                                   | enabled, negotiated by ALPN                           | One connection per server over TLS. Caddy already negotiates `h2`.                                                                                                                                           |
| HTTP/1.1                                 | keep-alive pool                                       | Plain-`http` LAN servers (ADR 0102 § 2) still reuse connections.                                                                                                                                             |
| `connect_timeout`                        | 10 s                                                  | A connection that cannot be made fails as `connect_timeout` instead of eating the whole deadline.                                                                                                            |
| `pool_idle_timeout`                      | 25 s                                                  | Below carrier NAT idle timers, some of which expire a mapping after about 30 s, and below most server idle timeouts (Caddy's is 5 minutes). Servers that close sooner are covered by § 7's fast-reset retry. |
| `http2_keep_alive_interval` / `_timeout` | 20 s / 10 s, only while streams are open              | Detects a dead connection under load without waking the radio when idle.                                                                                                                                     |
| TLS                                      | rustls with the same root store the plugin uses today | No change to which certificates are trusted (see "What this does not decide").                                                                                                                               |
| Proxy                                    | the same `reqwest` proxy features the plugin enables  | Parity by construction. `hyper-util` reads OS proxy settings only on macOS and Windows; on iOS and Android only proxy environment variables apply, for the old transport and the new one alike.              |
| Redirects                                | limited to 10; every hop must stay `http` or `https`  | Parity, plus the hop check the plugin only added (opt-in) in 2.7.0.                                                                                                                                          |
| Cookies                                  | none                                                  | Parity; `/v1` authenticates with bearer tokens.                                                                                                                                                              |
| `User-Agent`                             | `Axon/<version> (<os>)`                               | Server and proxy logs can tell the shell apart from browsers, and from versions of itself.                                                                                                                   |

One client serves every host.
`reqwest` pools per host, so a server URL change or a second account on another server needs nothing special.

### 2. Commands keep the plugin's request lifecycle

The command surface mirrors the plugin's, so abort and streamed bodies carry over without redesign:

- **`send`** registers the request under an id the JavaScript side chose, sends it, and resolves with `{ status, headers, url, version }` once headers arrive.
  Because the id is chosen before the call, a cancel can arrive before `send` returns.
- **`read_body`** pulls the next chunk, as the plugin's `fetch_read_body` does.
  Pull, not push, so `ReadableStream` backpressure still reaches the socket.
- **`cancel`** aborts a request at any stage.
  On HTTP/2, dropping a stream sends `RST_STREAM` and leaves the connection up for everything else.
  Under the plugin, every abort cost a connection.
- **`drop_body`** releases a response whose body the caller will not read.

Entries leave the table on body end, error, cancel or drop.
A response known to be empty is never stored at all: a null-body status (101, 103, 204, 205, 304), any response to `HEAD`, and `Content-Length: 0`.
A body that turns out to be empty in any other way (a chunked `200` with no data) leaves on the first `read_body`, which finds its end.
As a backstop, the binding registers each `Response` with a `FinalizationRegistry` that calls `drop_body`, so a response discarded unread still leaves the table once it is collected.
The number of live entries is exposed (§ 5), so a leak shows up as a number that keeps growing.

The JavaScript binding returns a real `Response`, with its `Content-Type` set at construction.
That retires the untyped-blob workaround in `media-service.ts`, which only exists because the plugin attaches headers afterwards.

### 3. Request bodies travel as raw bytes

`send` takes the body as Tauri's raw invoke payload (`InvokeBody::Raw`), with the request line and headers in invoke headers.
This replaces the JSON number array.
The body is still read whole into memory before sending, so streaming uploads stay out of scope.
But the copy is one buffer of the file's size, not an array and a JSON text several times larger.

### 4. Errors carry a fixed category

The Rust side walks the error's `source()` chain and reduces it to one of: `dns`, `refused`, `connect_timeout`, `tls`, `reset`, `body`, `cancelled`, `scope`, `other`.
`scope` is raised for a URL whose scheme is not `http` or `https`, and for a redirect hop that leaves them (§ 1).

It sends `{ kind, detail }`, and no free text.
`detail` comes from a closed set: an `io::ErrorKind` name such as `ConnectionReset`, a rustls `CertificateError` or alert variant such as `Expired` or `UnknownIssuer`, or nothing.
Error text from the chain is never forwarded.
`reqwest::Error::without_url` only removes the error's own URL, and DNS and TLS errors further down the chain can still name the host.
Since traces get pasted into bug reports, nothing that could carry a host name or URL crosses the IPC.

On the JavaScript side, the binding rejects with a `TypeError` whose `cause` carries `kind` and `detail`.
An abort the JavaScript side asked for is the exception.
When the caller's signal or a deadline fired, the binding rejects with the signal's reason, an `AbortError`, as `fetch` does, whatever the Rust side reported and in whichever order the two arrive.
So `api/client.ts`'s abort handling is unchanged, and an abort is never logged as `api:failed` or offered a retry.
`cancelled` is left for a cancel the transport decided on itself, such as a request still in flight when the webview is torn down.
`FailureKind` in `perf.ts` grows to those words, so `api:failed` records _why_ a request failed instead of `send`.
`asTransportFailure`'s string matching in `api/client.ts` goes away along with the plugin's strings.

### 5. Diagnostics measure the thing that changed

- **`version`** on every response (`h2`, `http/1.1`) is recorded on `api` traces, so a readout shows which protocol each request used.
- **A `stats` command** returns counters: connections opened (counted by a `connector_layer` around the connector), requests sent, retries by reason (`nack` and `fast_reset`, § 7), resets (§ 6), and live table entries.
  The counters live in the plugin's state, not in the client, so they carry on across a `reset`.
  The connector layer that counts connections is installed on every client the plugin builds.
  With performance instrumentation on, the client logs a `transport:stats` line on each room open and on resume.
  The fix is working when connections opened stays at one or two per resume, instead of tracking requests.
- **A developer toggle**, Settings → Debug → "Legacy shell transport", routes requests back through `tauri-plugin-http` for one release.
  It is there for A/B comparison on a device and as an escape hatch.
  It is off by default.

### 6. A fresh client after resume

When the app returns to the foreground, or the webview fires `online`, the binding calls `reset`.
That swaps in a newly built client, while requests already in flight finish on the old one.

Each table entry owns what its request needs, including a handle to the client it was sent on.
So `cancel` and `read_body` reach a request on the old client without consulting the current one, and the old client lives until its last entry leaves.
New requests always take the current client.

iOS tears down sockets while an app is suspended, and a WiFi-to-cellular handoff strands connections on the old interface.
A pooled connection that died silently is not "closed" as far as `hyper` knows, so the first request after resume would wait for a keepalive or the deadline.
Resetting costs one handshake per resume, which is cheap next to hundreds a minute.

### 7. Retry what provably never reached the server, and one narrow race

The transport retries a request automatically when the server is known not to have processed it.
That is safe for every method, `POST` included:

- **A pooled connection found dead before the request was written.**
  `hyper` already retries this (`retry_canceled_requests`, on by default).
- **HTTP/2 `REFUSED_STREAM`, or a graceful `GOAWAY` (`NO_ERROR`).**
  RFC 9113 § 8.7 guarantees the server did not process the stream.

`reqwest` 0.12.28's built-in default covers exactly those two HTTP/2 cases, with no budget and up to two retries per request.
That default (`retry::Builder::default`) is private to the crate, and a custom policy replaces it instead of extending it.
So the plugin builds its own policy, with the same two checks written out, so that each retry can be counted in `transport:stats` (§ 5).
A policy built through `reqwest::retry`'s public builder always carries `tower`'s `TpsBudget`: retries may add 20% to the requests seen in a 10-second window, with a floor of 10 retries a second.
The plugin also caps it at one retry per request (`max_retries_per_request(1)`).
The floor means a test that expects exactly one retry is never refused by the budget.

**One ambiguous case is also retried: the stale-connection race.**
A `GET` or `HEAD` that fails with `reset` before any response byte, within 1 s of being sent, is retried once on a fresh connection.
That signature is a reused connection the server closed just as the request left.
`hyper` avoids a connection whose close it has already seen, but not one closing at that moment.
The 25-second idle timeout in § 1 makes the race rare behind Caddy, but plain-`http` LAN servers often close idle connections much sooner (Apache's default is 5 s).
Browsers retry this case the same way, and today's connection per request never meets it at all, so leaving it unretried would be a regression.
A slower reset means network trouble and is not retried, since retrying would add load to the link that is failing, which is the problem this ADR exists to fix.
This retry lives in the plugin's send path rather than in the `reqwest` policy, because it depends on timing the policy cannot see.
It is counted separately (`fast_reset`), so field readings show how often it fires.

Nothing else is retried, because a connection that resets after the request was written may or may not have delivered it:

- **`POST`** is never retried on a reset.
  Message sends carry no idempotency key (the timeline offers a manual retry for a failed send), and the API spec says room creation is not idempotent.
- **`PUT`** is not retried either, although HTTP calls it idempotent.
  The shell's `PUT`s are device-state writes such as drafts and read markers, and a retried older write could land after a newer one and overwrite it.

Timeouts are not retried; the JavaScript deadlines own them.
Error statuses (5xx, 429, 503) are application policy, and belong with ADR 0122's load gate and the stores that already retry (media, favourites, push registration).

### 8. Authority is unchanged

The new commands accept any `http` or `https` URL on any host and port: the same reach the plugin's `http(s)://*:*` scope grants today.
The bearer token stays in JavaScript.
This is a transport change, not the narrower design ADR 0102 § 2 records (paths instead of URLs, and a credential the shell holds).

It is that design's prerequisite, though.
Once the transport is an in-repo command, narrowing it means changing the command's arguments, not replacing its plumbing.
That step stays sequenced with the keychain work ([issue 481](https://github.com/matrix-axon/matrix-axon/issues/481)), as ADR 0102 says, and is tracked in [issue 893](https://github.com/matrix-axon/matrix-axon/issues/893).
Until it lands, the any-URL reach exists through two paths during the toggle release: the old plugin and the new commands.

`tauri-plugin-http` and its capability entry are removed in the release after this ships, when the developer toggle goes.
The WebSocket stays on `tauri-plugin-websocket`: it is one long-lived connection, so it was never part of the problem.

## What this does not decide

- **Thread-root fan-out** (issue 662).
  Fewer requests is still worth doing; this ADR makes each request cheap, and that one makes them rarer.
- **HTTP/3.**
  `reqwest`'s `http3` support is behind an unstable build flag.
  HTTP/2 already gets the connection count to one.
- **Trusting the OS certificate store** (`rustls-platform-verifier`) for self-hosters with a private CA.
  That is a real request, but it changes which servers the app will talk to, and deserves its own decision.
- **Streaming uploads** from disk instead of from memory.

## Alternatives considered

**Upgrade `tauri-plugin-http`.**
Rejected: the current `v2` branch still builds a client per request.

**Patch the plugin upstream to cache its client.**
Worth offering upstream later, but not something to wait on.
Our needs go further than a cache: error categories, raw request bodies, connection counters and reset on resume.
A fork carries the plugin's whole surface (proxies, `data:` URLs, the scope machinery) to change one line of it, while the in-repo plugin is a few hundred lines that do exactly what `/v1` needs.

**Webview `fetch`, with server CORS.**
This would give the OS's own HTTP/2 and HTTP/3 stack.
ADR 0102 § 2 rejected it: a plain-`http` LAN server is mixed content from the shell's secure origin, and every operator would have to maintain a CORS list.
Nothing here changes that.

**A custom URI scheme that proxies to the server** (`register_asynchronous_uri_scheme_protocol`).
The webview would keep its native `fetch` and binary bodies.
Rejected for three reasons:

- Tauri does not tell the handler when the webview cancels, so an aborted request keeps running and holds its stream.
- Android's interception hook cannot read request bodies, so uploads would not work there.
- The scheme's origin differs by platform (`http://<scheme>.localhost` on Windows and Android), which brings back exactly the per-platform origin handling ADR 0102 avoided.

**Only reduce request volume** (issue 662, plus pacing).
It helps, but every remaining request still pays a handshake, and the app would stay one busy room away from the same cliff.

## Consequences

- One TCP and TLS connection per server, plus the WebSocket, instead of one per request.
  The hotel-network failure goes away.
  So does the per-request handshake latency on cellular.
- An abort no longer costs a connection.
- `api:failed` says why a request failed.
- Uploads stop inflating into JSON.
- The project owns a few hundred lines of transport and their tests, and tracks `reqwest` and `hyper` updates itself instead of through the plugin.
- On HTTP/2, all streams share one TCP connection, so loss on that connection stalls every request at once (head-of-line blocking at the TCP layer).
  Today each request fails alone.
  The keepalive in § 1 and the reset in § 6 bound how long a dead connection can hold things up.
- A large media download now shares the connection with API calls.
  HTTP/2 interleaves frames, so API calls are not queued behind it, but they do share its bandwidth.
- HTTP/2 lets the client have hundreds of streams open at once (Caddy allows 250 per connection by default).
  The client-side limits that exist today — thread roots six at a time, media through a permit pool — stay the throttle.
  ADR 0122's server-side load gate is unaffected.

## Delivery

1. This ADR (docs only).
2. The plugin, its JavaScript binding, the `Platform.fetch` swap behind the developer toggle, the new failure kinds, `version` on traces and `transport:stats`.
   All of it is in the web silo (`clients/web`, including `src-tauri`).
   It may land as two PRs: the Rust plugin with its tests, then the client wiring.
3. After one release with field readings: remove `tauri-plugin-http`, its capability entry and the toggle.

## Verification

- **Connection count, desktop.**
  Open a room with a few hundred threads, then count the shell's connections to the server.
  On macOS: `lsof -nP -iTCP -a -p "$(pgrep -x axon)"`.
  On Linux: `ss -tnp | grep axon`.
  Expect one to the server plus the WebSocket, where today's build shows a new connection per request in `TIME_WAIT`.
- **The failure mode, reproduced.**
  On a Linux test box, rate-limit new connections to a local axon:

  ```sh
  iptables -I INPUT -p tcp --syn --dport 8080 -m hashlimit --hashlimit-name axon --hashlimit-mode srcip --hashlimit-above 30/min --hashlimit-burst 20 -j DROP
  ```

  Today's build should show `api:late` near 35 s.
  The new build should show none, with `transport:stats` reporting one connection.

- **Device.**
  Traces show `version=h2`, and `transport:stats` connections opened stays at one or two per resume.
  The hotel-WiFi case is the acceptance test whenever one is available.
- **Abort.**
  A request aborted by its deadline frees its table entry (live count back to zero), and the next request reuses the connection (connections opened unchanged).
- **Resume.**
  Background the app for five minutes, then bring it back.
  The first request succeeds at normal latency, and one new connection is counted.
- **Proxy.**
  With a proxy configured in macOS or Windows system settings, the new transport and the legacy toggle both go through it.
  On iOS and Android, neither does.
- **Upload.**
  Send a file near 100 MiB on iOS and on the Android emulator, and watch memory stay near one copy of the file.
  Android's raw-payload IPC path differs from iOS's and has to be measured, not assumed.
- **Rust tests**, against an in-process server that counts accepted connections:
  - N requests share one connection, both over HTTP/1.1 keep-alive and over HTTP/2.
  - A cancel before headers and a cancel mid-body each release their entries.
  - Empty bodies never linger: a `HEAD`, a `204`, a `200` with `Content-Length: 0`, and a chunked `200` with no data each leave the live count at zero.
  - Across a `reset`, a request in flight on the old client can still be read and cancelled, and the counters keep their values.
  - Error classification gets the right `kind` for each case:
    - `refused`: a closed port (not a bound-but-not-listening socket, which times out on macOS instead of refusing).
    - `dns`: a `.invalid` host.
    - `tls`: a self-signed certificate.
    - `reset`: a server that closes the connection after reading the request.
  - `REFUSED_STREAM` is retried once for any method, and counted as `nack`.
  - A `GET` reset within 1 s is retried once and counted as `fast_reset`. A slower `GET` reset is not retried, and a `POST` or `PUT` reset is never retried.
  - An abort through the caller's signal rejects as `AbortError`, even when the Rust side's `cancelled` arrives first.
