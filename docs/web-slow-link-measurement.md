# Measuring a slow room open on a weak link (web client)

A newly-opened room can take tens of seconds to paint on a weak cell connection.
This is the procedure for collecting evidence that says **which** of several causes is responsible, because they need different fixes and the intuitive one is probably wrong.

The timeline page is ~17 KB and compressed (issue #86, PR #231).
17 KB does not take a minute on any link that is working at all, so the interesting question is not throughput.
It is whether the request is **stalled** or **starved**.

## What a cold room open does

`RoomPage`'s mount effect (`clients/web/src/pages/RoomPage.tsx`) fires four requests in one synchronous burst, unordered and unprioritized:

| Request                     | Size                                          | Gates the paint? |
| --------------------------- | --------------------------------------------- | ---------------- |
| `GET /v1/rooms`             | the whole room list                           | no               |
| `GET .../members`           | the whole member list — no `limit`, no cursor | no               |
| `GET .../threads`           | thread summaries                              | no               |
| `GET .../timeline?limit=50` | ~17 KB                                        | **yes, alone**   |

The pane shows "Loading messages…" until the fourth settles.
So the request that gates the paint is the smallest of the four, launched into contention with the largest two.
Behind them come an unbounded per-reply-target burst (`resolveReplyTargets`) and the media blob fetches.

There is also **no timeout, abort, or retry anywhere in `api/client.ts`**, so a stalled request rides the platform default — which on iOS Safari is on the order of a minute.

## The hypotheses

They predict different fixes, which is why this is measured rather than guessed.

|        | Claim                                                          | Signal in the readout                                                                                                                                  |
| ------ | -------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **H1** | Starvation — the timeline page queues behind the larger bodies | high `wait` on the timeline request; `net` landing after `list`/`members`                                                                              |
| **H2** | No timeout floor — a request stalls and nothing bounds it      | `phase: waiting`, or a `total` clustering near ~30 s / ~60 s rather than scaling with size; `outcome=pending` on an `:api` line, or any `api:deadline` |
| **H3** | Media and reply-target contention saturate the link            | many `/v1/media/*` and `/events/{id}` entries overlapping the open                                                                                     |
| **H4** | Reconnect loop re-triggers the mount fetches                   | `attempts` above 1                                                                                                                                     |
| **H5** | A head page arrives but the pane keeps its placeholder         | `heads` includes `applied` or `superseded` while `loading=true` and `rows` is null                                                                     |

H1 and H2 are the leading candidates and are not mutually exclusive.

## Reading the overlay

Enable **Settings → Debug → Performance instrumentation** (not `?perf=1`: that flag latches before any store exists and can silently mask the ordering, see ADR 0077).
Each cold room open then emits one summary line, up to four `:req` request lines, and up to four `:api` lines.

**Turn off "Show the live readout on screen" and turn on "Keep performance summaries on this device".**
The overlay only helps when someone is watching, and the loads worth capturing rarely happen while a screen recording is running.
That combination is what makes instrumentation usable during ordinary use: the summaries are recorded and kept in IndexedDB with no box of numbers over the app, and **Copy telemetry** puts them on the clipboard afterwards — paste them into a report instead of screenshotting.
Leave the readout on only when you intend to watch for something live.
Timings only: the marks that carry room and account identifiers are never written, and any identifier-shaped value is refused even from an allow-listed mark (`stores/telemetry.ts`).
Cleared on sign-out with the rest of the cache.

```
# session pl50usgl — 2026-09-25T13:30:05.205Z shell=browser display=standalone build=0.1.1+d3adb33
boot:room-open  phase=settled rows=980 net=940 q=780 conn=0 ttfb=120 xfer=40 reqs=31 kb=402 list=3120 pending=null members=8800 threads=610 people=4200 attempts=1 warm=false heads=applied@940 loading=false live=open reconnects=0 wall=2026-09-12T18:25:44.123Z
boot:room-open:req  route=accounts/{account}/rooms/{id}/members total=8800 wait=7900 conn=0 ttfb=120 xfer=780 bytes=41000 gzip=true proto=h2 cors=false
boot:room-open:api  route=accounts/{account}/rooms/{id}/timeline head=true at=2 hdr=130 total=938 outcome=ok stage=null status=200
```

**Check the session header first.**
`shell` says which client wrote the capture: `tauri` for the packaged app, `browser` for everything else, with `display` telling a home-screen web app (`standalone`) from a browser tab (`tab`).
The two share an icon, and they measure differently.
The packaged app sends `/v1` through `tauri-plugin-http`, outside the webview, so Resource Timing never sees those requests: in the shell the `:req` lines are absent and `q`/`conn`/`ttfb`/`xfer`/`reqs`/`kb` read `null`, and only the `:api` lines describe requests.
`build` is the release plus the build hash, which settles whether a fix was deployed before anyone debugs it.
Sessions recorded before these fields existed have a bare header.

**The first line alone is a complete reading.** `q`/`conn`/`ttfb`/`xfer` on it
decompose `net` for the head fetch — the one request that gates the paint — so a single screenshot from a phone is enough.
The `:req` lines below add the other requests it shared the link with.

- `rows` — when messages painted. **The user-visible number.**
- `net` — when the timeline page settled. The only request gating `rows`.
- `list` / `members` / `threads` — when the three fired beside it settled.
  **`net` landing well after these is H1.**
- `people` — member count, since that list is unpaginated.
- `stall` / `dns` / `tcp` / `tls` / `ttfb` / `hxfer` (on `boot:room-list`) — the **document fetch** decomposed, which on a poor real-world link is the single largest term in a cold start.
  `stall` is navigation start to the first DNS work, where a sleeping cell radio negotiating back onto the network lands; `tcp`/`tls` are connection setup, where a protocol negotiation that has to time out and retry shows up; `ttfb` is the server's think-time, which can be compared against the room-list and room-open figures on the same capture.
  Fast requests after a slow document mean the server was never the problem.
- `html` / `js` / `jskb` / `exec` (on `boot:room-list`) — startup decomposed.
  `html` is when the document arrived, `js` when the last script or stylesheet did, `jskb` what those cost on the wire, and `exec` the main-thread time after them: parse, execute, first render, service construction.
  **Every request waits on this**, so when `boot` dominates, none of the network findings apply and the cache cannot help — it is not read until `boot` has elapsed.
  A near-zero `jskb` means the assets came from the HTTP cache, which makes a large `exec` unambiguous.
- `q` / `conn` / `ttfb` / `xfer` — the head fetch's own phases. `q` is
  queueing, of which `conn` is connection setup:
  **`q` high with `conn` near zero is contention on an established connection (H1); `q` ≈ `conn` is a handshake on a slow link.** A large `ttfb` with a small `q` and an idle link is a stall, not contention (H2).
- `reqs` / `kb` — **every** `/v1/` request overlapping the open, and their
  total bytes.
  The three named above are not the whole link load:
  DM-title member lookups, the per-reply-target burst and media all ride the same connection, and a three-line readout cannot show thirty requests.
  A high `reqs` with a fast `net` is H3.
- `via` — which path filled the pane: `head` (the ordinary load), `jump` (an
  `?event=` deep link, whose load `RoomPage`'s mount effect hands to the jump effect), or `paint` (rows appeared with no fetch this recognised).
  A missing line usually means none of the three was reached.
- `attempts` — entry fetches for this one open. Above 1 is H4.
- `heads` — what each head load did, in the order they settled: `applied`, `superseded` (a sibling load won the race), `declined` or `failed`, followed by `pending` for each still in flight.
  Each outcome carries its time since the open began, as `failed@20100+applied@152950`.
  The time is what separates a load the 20 s request deadline failed from one that failed minutes later when the socket reconnected.
  `timeline:fetch:end` only says a request came back; this says whether its page was used.
  Only loads started during this open count: a load left over from an earlier visit to the same room can settle mid-open, and is not this open's.
  **`applied` or `superseded` with `loading=true` and no `rows` is H5.**
- `loading` — whether the pane was still showing "Loading messages…" at its last render (`null` if it never rendered).
- `live` / `reconnects` — the socket's state when the line was written, and how many times it reconnected during the open.
  A reconnect landing mid-open re-issues the head load, which is one way `attempts` reaches 2.
- `wall` — wall-clock time the line was written.
  **Use this, not the offset, to find the open in a server log.** iOS pauses the offset clock while the phone sleeps, so adding an offset to the session header's start time drifts by however long it slept.
- `warm` — a re-entry served from the ADR 0085 phase 1 store. **A warm line is
  not a cold open** and must not be read as one.
- `phase` — `settled`, or `waiting` if the timeline page had still not landed
  ten seconds in.
  **`waiting` is a result, not a broken readout**: it is what H2 looks like.
  A room that stays unsettled reports `waiting` again every 30 s, eight times in all (about four minutes), so a long stall shows its progress rather than a single point.
  Because that watchdog is ten seconds, wait at least twelve before screenshotting, or a stuck open shows no line at all.

On the request lines, `wait` is queueing (fetch started → bytes went out), `ttfb` is server think-time, and `xfer` is the transfer phase.

**Which of those carries the contention depends on `proto`.** Over `http/1.1`
the six-connection-per-origin limit makes contention show up as `wait`.
Over `h2` there is one multiplexed connection and no queue, so competing bodies interleave on the wire and lengthen **`xfer`** instead while `wait` stays near zero.
Reading `wait` alone on an h2 connection would find no contention no matter how starved the link was.

The timeline request is always listed, even when it was the fastest — it is the one that gates the paint, so it has to be comparable across conditions.
Note that several entries can share the route `.../rooms/{id}/timeline`: the room list resolves a preview per room and `shortRoute` collapses room ids.
The summary line's `q`/`conn`/`ttfb`/`xfer` are matched to the _room's own_ head fetch by settle time, so they are unambiguous where the `:req` lines are not.

`cors=true` means the timing breakdown was withheld (`wait`, `ttfb` and `xfer` read `null`), which happens when the API is a different origin with no `Timing-Allow-Origin`; the totals are still good.

### The client's own request lines

The `:api` lines come from the API client itself (`perfTraceRequest` in `src/perf.ts`), not from Resource Timing.
That makes two readings possible that the `:req` lines cannot give.
Resource Timing has no entry for a request until it finishes, so a request that is **still in flight** appears only here, which is exactly the request a `waiting` line is waiting on.
And in the packaged app these are the only request lines there are.

Each open lists every request for **this room's timeline** first, then any other request that failed or has not finished, up to four.
A request that finished normally and is not the room's timeline is left to the `:req` lines.

- `head` — whether this is the room's own timeline page rather than something it competed with.
- `at` — when the request started, since the open began.
- `hdr` — when its response headers arrived, since the request started; `null` if they never did.
- `total` — when it settled, since the request started; `null` while it is still in flight.
- `outcome` — `ok`, `timeout` (the 20 s API deadline), `aborted` (its caller gave up), `failed` (the transport refused), or `pending`.
- `stage` — for anything but `ok`, where the request was: `token` (waiting for an access token, which can itself be a network round trip), `headers`, or `body`.
- `status` — the HTTP status, once the headers are in.

**`outcome=pending stage=body` with a small `hdr` is a response whose headers arrived and whose body stalled.**
That was the 2026-09-25 stuck "Loading messages…": a timeline with its headers at 640 ms and its 6.7 KB body 107 s later.

Two marks stand alone, outside any room open, because either one is worth seeing whenever it happens:

- `api:deadline` — the request deadline failed a request: `route`, the `stage` it died in, `hdr` if its headers had arrived, and `after`, how long it had run.
- `api:late` — headers arrived for a request the client had already given up on.
  The transport ignored the abort and carried on.
  In WebKit that is expected, and the deadline holds only because the client races it (`fetchWithinDeadline` in `src/api/client.ts`).
  A run of these on a browser that should honor aborts is worth a report of its own.

## Getting a bad enough link

**Switching WiFi off in an area with good coverage will probably not reproduce
this.** Good LTE is fast.
The failure needs latency, loss, or congestion.

In rough order of usefulness:

1. **iOS Network Link Conditioner** — Settings → Developer → Network Link
   Conditioner, profile "3G" or "Very Bad Network".
   The Developer menu only appears once the device has been connected to Xcode at least once, so check it is there before planning around it.
2. **Safari Web Inspector** from a Mac over USB — gives a console and
   throttling, and makes the overlay a convenience rather than a necessity.
3. **Opportunistically**, on a genuinely bad connection, with a screen
   recording.
   This is what ADR 0077 built the overlay for.

**Record whether the capture came from Safari or the home-screen PWA.** They
are different containers on iOS: a PWA has its own HTTP cache _and_ its own IndexedDB, so its first launch is cold on both counts however much the same site has been used in the browser.
A `hydrate=null` from a PWA says nothing about whether the cache works in Safari, and a large `jskb` there is a first launch rather than a caching failure.

## What to capture

Per condition, **three cold opens** — the ADR 0085 device readings varied by about 2× between launches, and one reading cannot separate these hypotheses.
For each: the `boot:room-open` line — which is self-contained — plus the `boot:room-open:req` lines if the link load needs breaking down further.

A cold open means a fresh app launch into the room, not a re-entry: phase 1's warm store makes re-entry ~12 ms and measures something else entirely.

**The open has to race startup.** The largest measured delays came from opens
where the room list was _still in flight_ (`pending=list`); an open begun after startup traffic has drained measures an idle link and reports about a second.
Leaving Safari parked on the room URL and force-quitting it lands the next launch straight there, which is what removes the typing delay that otherwise lets startup finish first.

## The desktop lane

`clients/web/e2e/room-open-slow.spec.ts` runs the same readout under CDP network throttling (`throttleNetwork` in `e2e/perf-helpers.ts`, `slow-3g` profile) against the mock.
It cannot prove which hypothesis holds on a real link — a mock on loopback has no shared bottleneck — but it proves the readout
**discriminates** between the four requests, which is what makes a phone
reading trustworthy.

```bash
cd clients/web
pnpm build && pnpm exec playwright test e2e/room-open-slow.spec.ts --project=chromium
```
