# ADR 0075 — Cede the left-edge band to the browser's own swipe-back

## Context

ADR 0071 built a harness to chase a slow phone "back to room list" transition
and found a real cost — the un-windowed timeline — but could not reproduce the
case that actually prompted the report: an account with **six rooms and no long
timelines**, still slow on an iPhone 13. That cell measured ~47 ms under
Playwright's WebKit, against a reported delay closer to half a second. The
harness's own conclusion was that its blind spot was WebKit _and_ an
iPhone-13-class CPU together, and that the answer needed an on-device capture.

The capture (Safari Web Inspector Timelines, 11 s, two interactions, the second
of which the reporter confirmed reproduced the problem) says the blind spot was
not the one we thought.

### What the capture shows

Segmenting the recording by touch interaction and splitting the wait into busy
and idle time:

| interaction     | touch → first paint | busy  | idle   | longest stall |
| --------------- | ------------------- | ----- | ------ | ------------- |
| tap             | 103 ms              | 126   | ~0     | 3 ms          |
| swipe (9 moves) | **540 ms**          | 46 ms | 494 ms | 266 ms        |

**The app is not spending the time.** Our work — route, Preact render,
MutationObserver, style recalc — is finished 19 ms after `touchend`. Between
that point and the paint there is not one script record, not one layout record,
not one paint; main-thread CPU reads 0%. The only entries in the window are Web
Inspector's own polling. When the browser finally does produce a rendering
update, the room list measures and paints in ~20 ms.

That shape — idle main thread, no frames, then a resize — cannot be teardown
cost, which is what ADR 0071 predicted and what its harness was built to
measure.

### The cause: two navigations for one swipe

The swipe window contains three navigation events:

- `navigate` at +1 ms — our own `location.route('/')` from
  `navigateBackOneMobilePane`.
- `navigate` + **`popstate`** at +41 ms — a history traversal no app code
  requests. There is no `history.back()` anywhere in `clients/web/src`.

That second navigation is WebKit's own swipe-back committing on top of ours.
All nine `touchmove`s in the capture were `defaultPrevented` and it happened
anyway: WebKit's back gesture is a UIKit recognizer on the scroll view, not a
cancelable touch default, so the `preventDefault` in `handleTouchMove` never
called it off — despite a comment asserting it was "what actually stops the
browser's native edge-swipe-back."

With both navigations live, WebKit animates a snapshot of a page the app has
already replaced, and withholds rendering updates until that animation ends.
Hence a half-second stall with an idle main thread.

This also explains what ADR 0071 could not: the cost is a **fixed system
animation**, so it is constant regardless of room count or timeline length, and
it is invisible to any measurement of main-thread work. It is not WebKit-versus-
Chromium and not CPU speed; it is a gesture race that no headless lane
reproduces, because no headless lane has a UIKit gesture recognizer.

Two other findings from the same capture, both ruling things out:

- The `content-visibility: auto` fix on `.event-row` was **already deployed**
  when this trace was taken (verified in the served CSS bundle). It is not the
  answer to this particular delay.
- The reporter sees the same behavior in the installed PWA and in a Safari tab,
  so this is not a standalone-display-mode quirk.

## Decision

**Decline touches that start in the band the browser's own recognizer claims,
rather than trying to beat it.**

`handleTouchStart` gains a fourth refusal alongside the existing three (gesture
controls, horizontal scrollers, two-pane layout):

```ts
const NATIVE_BACK_EDGE_PX = 30
...
touch.clientX < NATIVE_BACK_EDGE_PX || ...
```

Inside the band the browser's back runs alone — which is the outcome we wanted
there anyway, and it paints throughout its animation instead of stalling.
Outside it, the custom handler is unopposed and stays immediate.

The comments claiming `preventDefault` defeats the native recognizer are
corrected in the same change. `preventDefault` still earns its place — it stops
scrolling and text selection fighting the pan — but not for the reason given.

### Alternatives rejected

- **CSS `touch-action`.** Cannot disable the gesture without also killing
  touch-scrolling for the code blocks and tables nested in the timeline, which
  need real horizontal panning. This is why the per-gesture opt-out exists.
- **Dropping the custom swipe entirely.** It works from anywhere on the screen,
  not just the edge, and it works where no history entry sits behind the room.
- **Staying passive in the band but routing on a short timer when no `popstate`
  arrives.** Acts only where the browser declined, so no race — but it is
  machinery for a case not yet observed. Held in reserve (see below).

## Consequences

- **Confirmed on-device.** The reporter confirms the transition is fixed. They
  also report that swiping from mid-screen is faster than swiping from the far
  left — exactly the residue the decision predicts: mid-screen takes our
  immediate route, the edge takes WebKit's animated transition, which is slower
  but painted throughout and reads as intentional rather than stalled.
- **Accepted regression: deep links.** A room opened from a `matrix.to` link or
  a PWA cold start has no history entry behind it, so the browser's gesture does
  not engage and an edge swipe there now does nothing. The Rooms button in the
  mobile room chrome is always present, and anyone whose swipe starts more than
  30 px in is unaffected. Accepted deliberately; if it turns up in practice, the
  timer variant above is the remedy.
- **The e2e lane cannot verify this and should not be expected to.** ADR 0071's
  harness measures main-thread marks under Chromium CPU throttling; this defect
  costs zero main-thread time and needs a UIKit gesture. Unit tests cover the
  guard (an edge-origin swipe must not navigate, and must leave the `touchmove`
  unclaimed); the behavior itself was verified on-device. A green harness run
  is not evidence about this class of bug.
- **ADR 0071's windowing follow-up still stands on its own merits.** This ADR
  explains the six-room report, not the timeline-length scaling the harness
  measured directly — WebKit is still ~3× slower than Chromium at long
  timelines even at native speed.
- **Method worth reusing.** For "slow but no JS", segment a Timelines capture by
  touch interaction, then split each wait into busy and idle time (union the
  record intervals — they nest, so summing durations double-counts). A wait
  dominated by idle means the browser withheld the frame and the app is not the
  culprit; counting `navigate`/`popstate` in the same window is what exposed the
  double navigation. The helper script used here lives under the gitignored
  `debug/` and is not part of the repo.
