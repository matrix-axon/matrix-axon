# ADR 0076 — Web timeline scroll stability: anchor the scroller, drop `content-visibility`

## Context

A phone user reported the timeline "jumping around" during scroll-back. The
jumps were real, reproducible on iOS 27 Safari (iPhone, 3× display), and did
**not** reproduce on desktop Chrome or Firefox.

Two mechanisms were at work, and separating them took most of the effort.

### Where the shifts came from

`.event-row` carried `content-visibility: auto` with
`contain-intrinsic-size: auto 2.5rem`, added under ADR 0071's diagnosis that
the phone "back to room list" transition scaled with timeline length. A skipped
row's height is a *guess* until it renders, and 2.5rem is wrong for anything
but a one-line message: a wrapped body, a reaction row, or an inline image is
50–130px taller. Scrolling back means crossing rows that have never rendered,
so each correction lands **above** the reader and shoves everything down.

Instrumentation (ADR 0077) measured the real magnitudes on-device:
`moved=68, 70, 133, 60, 84` — matching the visible lurches one-for-one.

### Why the browser did not absorb it

Scroll anchoring is supposed to. It is uneven in practice: WebKit only shipped
it in Safari 27 (STP 238, Feb 2026), so it is absent on most iOS in the field,
and where present it picks its anchor by heuristic with no view on which row the
reader cares about.

### The measurement method

Video, not intuition. A screen recording is decomposed to frames and the
frame-to-frame displacement recovered by 2D phase correlation. Two cautions,
both learned the hard way:

- **1D row-mean profiles alias.** Message rows are quasi-periodic (~50px
  pitch), so a 1D correlation happily locks onto the wrong multiple. 2D
  correlation over the actual text pixels does not.
- **Displacement is only meaningful on near-still frames.** While the timeline
  is moving, a shift cannot be distinguished from scrolling; and frames whose
  correlation peak collapses (< 0.5) share too little content to compare at all
  — that is a page landing or a slice replacement, not a displacement.

## Decision

**1. Anchor the scroller ourselves** (`RoomPage`, `.timeline` gets
`overflow-anchor: none`). Hold the topmost fully visible row; when a
`ResizeObserver` on the event list fires, put back whatever moved it.
`ResizeObserver` runs before paint, so the correction lands in the same frame.

Three details are load-bearing:

- **The measurement is scroll-invariant.** A row's viewport-relative top is
  `offsetInContent − scrollTop`, so between observations
  `grownAbove = (topNow − topThen) + (scrollTopNow − scrollTopThen)`. The
  anchor is therefore captured **once** and survives the reader's scrolling.
  Re-capturing per scroll event is not merely wasteful, it is *wrong*: reading
  geometry forces layout, layout is where `content-visibility` decides to
  render incoming rows, so the capture triggers the growth it means to measure
  and records the position after it. That defect made corrections silent
  through an entire scroll-back while working perfectly at rest.
- **The anchor is the first row entirely at or below the top edge**, not the one
  straddling it. A straddling row's top does not move when it grows downward.
- **Anchor drift is bounded** to 1.5 viewport heights; growth between the anchor
  and the visible rows would otherwise go uncounted.

**2. Remove `content-visibility: auto` / `contain-intrinsic-size` from
`.event-row`.** ADR 0075 found the actual cause of the slow phone (racing
WebKit's own swipe-back) and fixed it, so the optimisation had nothing left on
its side of the trade — while costing a 50–130px shift per never-rendered row.

## What was measured

Consistent method across six recordings of the same gesture. "Still-frame max"
is the largest displacement while the timeline is stationary; "motion spikes"
are residuals above the local scroll trend.

| Build                                | still-frame max | motion spikes > 20px | max residual |
| ------------------------------------ | --------------- | -------------------- | ------------ |
| before anchoring                     | 85 px           | 4                    | 85 px        |
| anchoring only                       | 3 px            | 6                    | 85 px        |
| anchoring, `content-visibility` gone | 3 px            | 1                    | —            |
| final                                | 3 px            | **0**                | **11 px**    |

## Hypotheses that were wrong

Recorded because each cost a cycle, and because a future reader will be tempted
by the same ones:

1. **WebKit lacks scroll anchoring.** Refuted by the reporter's user agent —
   iOS 27 / Safari 27 has it.
2. **The anchor straddling the viewport top.** Real defect, fixed, but changed
   nothing measurable.
3. **iOS inertial scrolling overriding `scrollTop`.** Refuted by reading the
   value back after writing it: `applied === requested`, every time.
4. **The correction itself was the jump.** Refuted by a run where corrections
   were suppressed entirely — the spikes persisted unchanged.

The pattern: every hypothesis about *the browser* was wrong, and every defect
was in our own code. Prefer instrumenting the app over theorising about the
engine.

## Consequences

- Every row is laid out again, so a long history costs more than it did. The
  600-event retained-slice bound (same change set) caps that, which it did not
  when ADR 0071 was written. `pnpm test:e2e:perf` re-runs the lane if the trade
  needs re-checking.
- **Windowing (#26) is still the way back to a bounded row count**, and is now
  the only outstanding lever. A windowed timeline with measured heights makes
  position deterministic rather than corrected, and would supersede the
  anchoring here rather than sit beside it.
- `RoomPage` now owns scroll position during content changes. A future
  regression will present as *drift or fighting*, not lurching — a different
  signature to look for.
- One residual artifact: reaching the very start of a room prepends a large
  final page, and the resulting ~1700px correction shows a one-to-two frame
  blank while WebKit rasterises the newly exposed region. The correction is
  right (it preserves the reader's position); the flash is repaint latency and
  cannot be made smaller without abandoning position preservation.
- The scroll-back chain, retained-slice bound, and anchoring interact: progress
  is measured by the *cursor advancing*, never by the slice growing, because at
  the retained cap a page prepends 50 and drops 50 for no net change.
