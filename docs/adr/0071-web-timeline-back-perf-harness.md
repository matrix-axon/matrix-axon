# ADR 0071 — Web perf harness: timeline → room-list transition

## Context

One user (iPhone 13) reports that switching from an open room's timeline back to
the room list is slow. It is not reproducible on newer hardware (iPhone 15 Pro
Max), and a round of Safari-devtools tracing with the user was unproductive. We
wanted a way to reproduce and attribute the slowness _in-repo_, on demand,
without asking the user to trace again.

The app is already instrumented for exactly this. `src/perf.ts` emits `?perf=1`-
gated `performance.mark`s across the whole transition — `room-list:visible-
compute:*`, `room-list:measure:*`, `room-list:render`, `room-list:post-render`
(a `perfMarkFrames` frames-to-paint span), plus `shell:*`. What was missing was
(a) a way to drive the transition under a slower-than-desktop CPU, (b) a large-
timeline fixture, and (c) anything that reads the marks back.

### What the reporter's profile tells us

The affected user has **~150 rooms, room-list previews OFF, and a long timeline**
in the room they switch away from. At ~150 rooms the room-list hot paths are
cheap (`sortRooms`/`filterRooms` and the read-marker key string in `RoomList`),
and previews off means no per-row `hydratePreview`. That points away from the
room list and at the **un-windowed timeline**: `RoomPage` renders a row per event
(`visible.map(... <MessageEventRow/>)`, no virtualization), so switching to the
list unmounts that entire subtree — every row and its cleanup effects —
synchronously, on the same tick the sidebar re-renders. That is main-thread work
that scales with timeline length, which is why a slower phone feels it.

## Decision

Add an **opt-in Playwright perf lane** that reproduces the switch under an
emulated slow CPU across a sweep of timeline lengths, holding rooms at 150 and
previews off, and reads the app's own marks back into a phase breakdown.

- **Fixture.** `e2e/mock-server.mjs` gains a `bulk-timeline` knob
  (`POST /__e2e/bulk-timeline?count=N`) mirroring the existing `bulk-rooms`: the
  timeline GET prepends `N` synthetic messages so `RoomPage` mounts a long, un-
  windowed list. Zero by default, so every other spec is untouched.
- **Slow-device emulation.** `throttleCpu` (`e2e/perf-helpers.ts`) uses the CDP
  `Emulation.setCPUThrottlingRate`. Mid-range phones run ~4–6× slower than the
  desktop Chromium the lane runs on; the sweep covers 1×, 4×, 6×.
- **Measurement.** The spec brackets the transition with an `axon:e2e:back-start`
  mark, taps the phone "Rooms" affordance, waits for the list, and derives a
  breakdown (`phaseBreakdown`): `total` (start → last list render), the room-list
  phase (compute + measure), render-pass count, and the post-render frame span.
- **Opt-in.** Perf runs are slow and machine-sensitive, so the lane is
  `test.skip` unless `PERF=1` (`pnpm test:e2e:perf`); it stays out of the default
  `test:e2e` sweep and off CI's critical path.

The lane asserts the hypothesis _directionally_ (never on absolute ms): at the
slowest CPU, `total` grows materially with timeline length while the room-list
phase grows far less — leaving `RoomPage` teardown as the cost that scaled.
The two endpoint cells used for those assertions are sampled three times and
compared by median. The room-list check uses its absolute share of the total
increase rather than a ratio between near-zero phase timings, so sub-millisecond
jitter cannot dominate the result. The timing matrix is attached before the
assertions run so a failure retains all of its diagnostic evidence.

### What the harness found

Running it confirms the diagnosis (rooms=150, previews off; `total` in ms):

| CPU | timeline 50 | timeline 500 | timeline 2000 | room-list phase |
| --- | ----------- | ------------ | ------------- | --------------- |
| 1×  | ~112        | ~65          | ~110          | ~0              |
| 4×  | ~77         | ~274         | ~549          | ~0              |
| 6×  | ~105        | ~279         | ~690          | ~0–1.4          |

At native CPU the transition is flat regardless of timeline length (the 15 Pro
Max case). Under throttle it scales hard with timeline length, while the room-
list phase stays pinned near zero throughout. The cost is the un-windowed
`RoomPage` teardown/re-render, not the room list.

### A WebKit lane

A later report reproduced the slowness on an account with only **six rooms and
no long timelines** — which the timeline-scaling story above cannot explain, and
which a Chromium run clocks as snappy. Since iOS Safari is WebKit, not Blink, the
lane also runs under **Playwright's WebKit** (gated on `PERF`, scoped to this
spec, iPhone 13 profile minus the WebKit-unsupported `isMobile`). CDP CPU
throttling is Chromium-only, so WebKit runs at native speed; both engines always
measure the reported ~6-room cell after a discarded warmup, so the runs compare.

Two results, warm (`total` / post-render frames-to-paint, ms):

| cell                | Chromium | WebKit   |
| ------------------- | -------- | -------- |
| reported (~6 rooms) | 54 / 14  | 47 / 20  |
| timeline 2000       | 103 / 0  | 329 / 27 |

- **The six-room slowness did not reproduce on either engine.** Warm, WebKit is
  as fast as Chromium at that config (~47 ms). An early ~206 ms WebKit reading was
  cold-start warmup, not a steady-state gap — hence the mandatory warmup pass.
- **At long timelines WebKit is ~3× slower than Chromium even at native speed**,
  so the un-windowed-timeline penalty bites far harder on real iOS than the
  Chromium sweep implied — added weight behind the windowing follow-up.

The lane's structural blind spot is the one config that matches the reporter:
**WebKit _and_ an iPhone-13-class CPU together.** WebKit cannot be CPU-throttled
here, so that combination is unmeasurable; the six-room cause is being chased
with a scoped on-device Safari Timelines capture instead.

## Consequences

- We can reproduce and re-measure this class of slowness without the user, and
  the lane doubles as a regression guard for the transition.
- **Follow-up (separate PR, per the one-silo rule):** window the timeline in
  `RoomPage` — mirroring `virtual-window.ts`, already used by the room list —
  and/or defer/chunk `RoomPage` teardown so the list can paint first. This ADR
  deliberately lands only the diagnostic; the fix is scoped on its own.
- The `bulk-timeline` knob is a test-only control surface under `/__e2e`, the
  same trust boundary as `bulk-rooms`.
- The WebKit lane is kept as a permanent regression tool: it is the only place
  the app runs under Apple's engine, and it is what surfaced the 3× iOS timeline
  penalty. It runs only under `PERF=1`, so it never touches the default sweep.
