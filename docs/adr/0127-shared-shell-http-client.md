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

| Setting                                  | Value                                                                     | Why                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| ---------------------------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| HTTP/2                                   | enabled, negotiated by ALPN                                               | One connection per server over TLS. Caddy already negotiates `h2`.                                                                                                                                                                                                                                                                                                                                                                                |
| HTTP/1.1                                 | keep-alive pool                                                           | Plain-`http` LAN servers (ADR 0102 § 2) still reuse connections.                                                                                                                                                                                                                                                                                                                                                                                  |
| `connect_timeout`                        | 10 s                                                                      | A connection that cannot be made fails as `connect_timeout` instead of eating the whole deadline.                                                                                                                                                                                                                                                                                                                                                 |
| `pool_idle_timeout`                      | 25 s                                                                      | Below carrier NAT idle timers, some of which expire a mapping after about 30 s, and below most server idle timeouts (Caddy's is 5 minutes). Servers that close sooner are covered by § 7's fast-reset retry.                                                                                                                                                                                                                                      |
| `http2_keep_alive_interval` / `_timeout` | 20 s / 10 s, only while streams are open                                  | Detects a dead connection under load without waking the radio when idle.                                                                                                                                                                                                                                                                                                                                                                          |
| TLS                                      | rustls with the same root store the plugin uses today                     | No change to which certificates are trusted (see "What this does not decide").                                                                                                                                                                                                                                                                                                                                                                    |
| Proxy                                    | the same `reqwest` proxy features the plugin enables                      | Parity by construction. `hyper-util` reads OS proxy settings only on macOS and Windows; on iOS and Android only proxy environment variables apply, for the old transport and the new one alike.                                                                                                                                                                                                                                                   |
| Redirects                                | at most 10, followed by the plugin; every hop must stay `http` or `https` | Parity, plus the hop check the plugin only added (opt-in) in 2.7.0. The plugin follows them so each hop can take the client its origin calls for (below). Hops follow `fetch`'s method and credential rules: a cross-origin hop drops `Authorization`, `Cookie` and `Proxy-Authorization` and sends `Origin: null`, and other headers carry on, as in `fetch`. No hop carries a `Referer`: `reqwest`'s is the whole previous URL, query included. |
| Cookies                                  | none                                                                      | Parity; `/v1` authenticates with bearer tokens.                                                                                                                                                                                                                                                                                                                                                                                                   |
| `User-Agent`                             | `Axon/<version> (<os>)`                                                   | Server and proxy logs can tell the shell apart from browsers, and from versions of itself.                                                                                                                                                                                                                                                                                                                                                        |

One client serves every host.
`reqwest` pools per host, so a server URL change or a second account on another server needs nothing special.

**A second client for origins known to speak HTTP/2** (added during implementation).
Negotiating by ALPN means `hyper-util` cannot know a server speaks HTTP/2 until a handshake has finished.
Until then, a burst of requests to a cold pool opens one TCP and TLS connection per request.
The first to negotiate `h2` is kept and the rest are discarded, but every one of them was a handshake.
Only a client built HTTP/2-only takes `hyper-util`'s per-origin connecting lock, which holds every request in the burst until the first connection is ready.
Measured against an in-process server, ten concurrent requests after a `reset` opened ten connections on the negotiating client and one on the HTTP/2-only client.
Since § 6 resets on resume, that difference repeats with every resume.

So the plugin builds two clients with the settings above.
One negotiates by ALPN, offering `h2` and `http/1.1`.
The other offers only `h2`.
An `https` origin moves to the HTTP/2-only client once a response from it arrives over HTTP/2.
It moves back when the server shows it stopped offering HTTP/2.
A `no_application_protocol` alert, refusing an offer of `h2` alone, says so at once, and costs one request.
A server that ignores the ALPN offer is less clear: it answers the HTTP/2 preface with HTTP/1.1, which reads as a `reset`, the same as a WiFi-to-cellular handoff ending a working connection.
Moving back on a handoff would send the next room's burst through the negotiating client, one new connection per request, on the network least able to take it (added in review).
So any other failure once a connection existed is a strike, and only a second one moves the origin back, from a request sent after the first failed.
Every request on a connection a handoff ends was sent before the first of them failed, so they count once, and the next request goes out on a new connection and clears the strike by getting a response.
A server that really stopped offering HTTP/2 costs two failed requests.
A cancel, `connect_timeout`, `refused`, `dns` or a reset while connecting leaves the origin where it is and adds no strike: those say nothing about the server, and on a network that drops new connections, moving back would bring back the burst.
The set of known origins describes servers, not connections, so it survives a `reset`.
Each redirect hop takes the client its own origin calls for, so a known HTTP/2 origin can redirect to one that speaks only HTTP/1.1.
Plain `http` never moves, because there is no ALPN to negotiate.

**The known origins are kept between launches** (added after device testing).
Without that, every launch was a first contact: the burst on the negotiating client, then a second connection for the HTTP/2-only client straight after it.
On a network that drops new connections, that second connection is the one most likely to be dropped, and every request waits behind it.
The plugin keeps the set in a small file in the app's cache directory, written when an origin is learned or forgotten, on the blocking pool so no request waits for the disk.
It holds origins only, the servers already signed in to.
On reading back it keeps only well-formed `https` origins, at most 64, and treats an unreadable file as empty, since losing it costs one connection.
So only the first launch against a server pays for two connections.

### 2. Commands keep the plugin's request lifecycle

The command surface mirrors the plugin's, so abort and streamed bodies carry over without redesign:

- **`send`** registers the request under an id the JavaScript side chose, sends it, and resolves with `{ status, headers, url, version }` once headers arrive.
  Because the id is chosen before the call, a cancel can arrive before `send` returns.
  A cancel that arrives before its `send` is remembered for 30 seconds.
  The method is normalised as `fetch` normalises it: `get` is `GET`, and `CONNECT`, `TRACE` and `TRACK` are refused.
- **`read_body`** pulls the next chunk, as the plugin's `fetch_read_body` does.
  Pull, not push, so `ReadableStream` backpressure still reaches the socket.
- **`cancel`** aborts a request at any stage.
  On HTTP/2, dropping a stream sends `RST_STREAM` and leaves the connection up for everything else.
  Under the plugin, every abort cost a connection.
- **`drop_body`** releases a response whose body the caller will not read.

Entries leave the table on body end, error, cancel or drop, and when their webview starts loading a page or its window is destroyed: the page that could read or cancel them is gone.
A response known to be empty is never stored at all: a null-body status (101, 103, 204, 205, 304), any response to `HEAD`, and `Content-Length: 0`.
`send` says so with `body: false`, so the binding builds a null-body `Response` without repeating the rule.
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

**Measured on devices, that holds only for the page's own copy** (added after device testing, #921).

- On iOS, WebKit's custom-protocol fetch and wry/Tauri's conversion of the body add copies of their own. With a file near the 100 MiB upload cap, Axon peaked at about 4× the file size and WebContent at about 5.5×. Axon returned to its baseline afterwards.
- Android has no raw invoke body at all. Tauri falls back to `postMessage`, which serialises the bytes as a JSON number array. A 95 MiB upload grew the emulator's WebView renderer to 929 MiB, and Android killed it.
- The legacy transport sends JSON on every platform, so neither is a regression.
- Sending large bodies in chunks is the fix on both platforms (#921).

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
- **A `stats` command** returns counters: connections opened (counted by a `connector_layer` around the connector), requests sent, retries by reason (`nack`, `fast_reset` and `requeue`, § 7), resets (§ 6), and live table entries.
  The counters live in the plugin's state, not in the client, so they carry on across a `reset`.
  The connector layer that counts connections is installed on every client the plugin builds.
  With performance instrumentation on, the client logs a `transport:stats` line on each room open and on resume.
  The fix is working when connections opened stays at one or two per resume, instead of tracking requests.
- **A developer toggle**, Settings → Debug → "Legacy shell transport", routes requests back through `tauri-plugin-http` for one release.
  It is there for A/B comparison on a device and as an escape hatch.
  It is off by default.

### 6. A fresh client after resume

When the app returns to the foreground after at least 5 s hidden, or the webview fires `online`, the binding calls `reset`.
That swaps in newly built clients (both of § 1's), while requests already in flight finish on the old ones.
The origins known to speak HTTP/2 are kept, so the first burst after resume opens one connection.

Each table entry owns what its request needs, including a handle to the client it was sent on.
So `cancel` and `read_body` reach a request on the old client without consulting the current one, and the old client lives until its last entry leaves.
New requests always take the current client.

iOS tears down sockets while an app is suspended, and a WiFi-to-cellular handoff strands connections on the old interface.
A pooled connection that died silently is not "closed" as far as `hyper` knows, so the first request after resume would wait for a keepalive or the deadline.
Resetting costs one handshake per resume, which is cheap next to hundreds a minute.

Not every return is a resume (added in review).
A shorter absence, an app switch and straight back, comes before iOS suspends the app, so the pool is kept.
The absence is timed with `Date.now()`, since iOS stops `performance.now()` while the app is suspended.
A return and `online` often arrive back to back, and a reset within 2 s of another is skipped, since it would only discard the connection the first just opened.
The `transport:stats` line is written on every return either way, and its `resets` count says whether one ran.

### 7. Retry what provably never reached the server, and one narrow race

The transport retries a request automatically when the server is known not to have processed it.
That is safe for every method, `POST` included:

- **A pooled connection found dead before the request was written.**
  `hyper` already retries this (`retry_canceled_requests`, on by default).
- **HTTP/2 `REFUSED_STREAM`, or a graceful `GOAWAY` (`NO_ERROR`).**
  RFC 9113 § 8.7 guarantees the server did not process the stream.

`reqwest` 0.12.28's built-in default covers exactly those two HTTP/2 cases, with no budget and up to two retries per request.
Nothing outside `reqwest` sees those retries happen, and a classifier in a custom policy runs on every attempt, including ones the policy will not retry, so counting there overcounts.
So the plugin turns `reqwest`'s retries off (`retry::never()`) and retries the two cases itself, once per request, counting each retry as it is sent (`nack`, § 5).
Like `reqwest`'s default, it has no budget: the server never processed the refused stream, and one retry per request bounds the extra load.

**One ambiguous case is also retried: the stale-connection race.**
A `GET` or `HEAD` that fails with `reset` before any response byte, within 1 s of being sent, is retried once on a fresh connection.
That signature is a reused connection the server closed just as the request left.
`hyper` avoids a connection whose close it has already seen, but not one closing at that moment.
The 25-second idle timeout in § 1 makes the race rare behind Caddy, but plain-`http` LAN servers often close idle connections much sooner (Apache's default is 5 s).
Browsers retry this case the same way, and today's connection per request never meets it at all, so leaving it unretried would be a regression.
A slower reset means network trouble and is not retried, since retrying would add load to the link that is failing, which is the problem this ADR exists to fix.
It lives in the same place, and a request is retried at most once, whichever case it meets.
It is counted separately (`fast_reset`), so field readings show how often it fires.

Nothing else is retried, because a connection that resets after the request was written may or may not have delivered it:

- **`POST`** is never retried on a reset.
  Message sends carry no idempotency key (the timeline offers a manual retry for a failed send), and the API spec says room creation is not idempotent.
- **`PUT`** is not retried either, although HTTP calls it idempotent.
  The shell's `PUT`s are device-state writes such as drafts and read markers, and a retried older write could land after a newer one and overwrite it.

**A request that never left is sent again: the requeue** (added after device testing).
The HTTP/2-only client holds every request behind one connection attempt.
When that attempt ends without a connection (it failed, or the request that started it was cancelled, as a room switch does), `hyper-util` drops the waiting requests with a private pool error ("request was canceled").
Its own retry for canceled requests does not cover that error, so each waiting request failed as `other`.
Measured against an in-process server, cancelling the first of five requests 100 ms into a slow handshake failed the other four.
Nothing was written for those requests, so the plugin sends them again, for any method.
One of them starts the next connection attempt, and the rest wait behind it, until each request's own deadline cancels it.
The first requeue goes at once, since the attempt was most often cancelled by its own caller and the network is fine.
After that a request waits 50 ms, doubling each time to at most 2 s, so a network that resets every new connection gets a slowing trickle of attempts rather than one after another (added in review).
These are counted separately (`requeue`), and do not use up the one retry above.
A request has one budget of 64 requeues across that retry, a backstop the pauses put well past any deadline.

The requeue matches `hyper-util`'s error text, since nothing public identifies the error.
It is not pinned to one `hyper-util` version, which would hold back the whole shell's dependencies.
The Rust tests that assert requeues happen run on any change to the shell's `Cargo.lock` instead, and if the text changed unnoticed, waiting requests would fail as `other`, as before.

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
  On the first launch against a server, the negotiating client's connection is briefly a second one; it closes after the 25-second idle timeout. Later launches start from the stored origins (§ 1) and open one.
- **The failure mode, reproduced.**
  On a Linux box running the shell, drop new outgoing connections above a rate, as the hotel gateway did, to a server behind TLS (so the new transport speaks HTTP/2):

  ```sh
  iptables -I OUTPUT -p tcp --syn -d <server IP> --dport 443 -m hashlimit --hashlimit-name axon --hashlimit-mode dstip --hashlimit-above 30/min --hashlimit-burst 20 -j DROP
  ```

  The legacy transport stalls: waves of `api:deadline stage=headers` at the 20 s deadline, and rooms left at `heads=pending`.
  (The iPhone's `api:late` near 35 s does not reproduce here: there WebKit lost the deadline's abort, so the stalled connect finished on Darwin's SYN schedule, while on WebKitGTK the abort reaches the plugin and the connect is dropped.)
  The new transport shows no stalls, and `transport:stats` connections opened stays flat across room opens.
  Measured 2026-11-01 against a local Caddy: the legacy transport stalled a room open for 20 s, and the new one opened about twenty rooms in 11 s on three connections.

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
  - After a `reset`, a burst of concurrent requests to an origin known to speak HTTP/2 opens one connection, and an origin that stops offering HTTP/2 goes back to negotiating: at once on a refused ALPN offer, on the second failure from a server that ignores the offer.
  - An HTTP/2 connection that closes with two requests on it costs one strike, and the origin keeps its place.
  - A first-contact burst whose later connects stall shares the first connection to complete.
  - Requests waiting behind an HTTP/2 connect are sent again when it is cancelled or fails, rather than failing as `other`.
  - An origin learned in one launch opens one connection in the next, a forgotten one is forgotten in the store too, and the store reads back only `https` origins.
  - A redirect from a known HTTP/2 origin reaches an HTTP/1.1-only one, and both origins keep their places.
  - Redirect hops rewrite methods, drop credentials and send `Origin: null` across origins, as `fetch` does.
  - A cancel before headers and a cancel mid-body each release their entries.
  - A cancel that arrived first stops its `send`, unless it is more than 30 seconds old.
  - A cancelled request's task, waking after its id was reused, leaves the new request's entry alone.
  - Empty bodies never linger: a `HEAD`, a `204`, a `200` with `Content-Length: 0`, and a chunked `200` with no data each leave the live count at zero.
  - Across a `reset`, a request in flight on the old client can still be read and cancelled, and the counters keep their values.
  - Error classification gets the right `kind` for each case:
    - `refused`: a closed port (not a bound-but-not-listening socket, which times out on macOS instead of refusing).
    - `dns`: a `.invalid` host.
    - `tls`: a self-signed certificate.
    - `reset`: a server that closes the connection after reading the request.
  - `REFUSED_STREAM` is retried once for any method, and counted as `nack`; a refused retry is counted once and not retried again.
  - A `GET` reset within 1 s is retried once and counted as `fast_reset`. A slower `GET` reset is not retried, and a `POST` or `PUT` reset is never retried.
  - An abort through the caller's signal rejects as `AbortError`, even when the Rust side's `cancelled` arrives first.
