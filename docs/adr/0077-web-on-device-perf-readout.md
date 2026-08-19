# ADR 0077 — On-device performance readout: marks a phone user can actually send

## Context

ADR 0071 instrumented the web client with `?perf=1`-gated `performance.mark`s
and built a Playwright lane that reads them back. That lane answers questions
about *a machine we control*. It cannot answer questions about **the device
that is actually slow**, and the reports that matter come from phones.

Three obstacles, each of which cost real time before being addressed:

- **iOS Safari has no on-device console.** Reading marks from an iPhone
  otherwise means tethering it to a Mac running Web Inspector — a setup step
  that turns "send me a recording" into a project.
- **`?perf=1` is fragile.** `perfEnabled()` latches its answer on the first
  call, which happens during app boot. If the URL carried the parameter but the
  app booted elsewhere first (root, a redirect, a client-side navigation), the
  flag is cached `false` for the life of the page and appending it afterwards
  does nothing. The e2e lane sidesteps this by setting `sessionStorage` in an
  init script; a human editing a URL on a phone has no such escape.
- **Raw marks are unreadable.** A session accumulates thousands. Even displayed,
  they scroll past faster than a recording can capture.

This mattered concretely: a scroll-stability bug (ADR 0076) went through four
wrong hypotheses, three of them about browser behavior, because the app's own
measurements were unreachable from the device exhibiting the problem. The first
on-device readout falsified two of them immediately.

## Decision

**1. A Settings toggle.** `settings.perfMarks` (schema, defaults, parser,
signal) drives `setPerfEnabled`, which writes the same session flag. `?perf=1`
still wins for a single session and still serves the e2e harness. Turning it
off clears the readout so no stale tail is left on screen.

**2. An on-screen readout** (`PerfOverlay`) drawing the tail of selected marks
over the app: fixed, `pointer-events: none`, outside the timeline's layout so
watching cannot perturb what is watched. Because the user is screen-recording
anyway, the numbers and the behavior they explain land **in the same frames**,
which is what makes them correlatable at all.

Only curated marks are mirrored — the scroll anchor, the paging that feeds it,
and transition summaries. Everything else stays in the full timeline, which it
would otherwise crowd out of a ten-line buffer.

**3. A phase summary.** A back gesture arms a timer; 800ms later the marks are
reduced to one line and emitted as `transition:back`:

```
26344 transition:back total=412 list=8 renders=3 frames=16 rooms=150
```

Deliberately the same vocabulary as `phaseBreakdown` in
`e2e/perf-helpers.ts` — `total` (gesture → last list render), `list`
(compute + measure, the room-list phase), `renders` (pass count, so a
re-render storm is visible), `frames` (post-render frames-to-paint), `rooms`.
An on-device report and a CI run are therefore directly comparable.

**4. A bounded in-memory mark log** (400 entries) rather than reading back
`performance.getEntriesByType('mark')`. Engines cap and evict entries from the
performance timeline, and a summary that silently loses its inputs is worse
than no summary. It also makes the reduction testable, which the browser
timeline is not under jsdom.

## The diagnostic loop

For a future agent handed "the app feels slow/jumpy on my phone":

0. **Try to reproduce it scripted first.** `playwright.config.ts` carries a
   WebKit project at the iPhone 13 profile, on Linux, no device required.
   Layout bugs are input-independent and can be measured directly from
   `getBoundingClientRect()` — orders of magnitude faster to iterate on than
   the loop below, which should be reserved for what only a device shows
   (CPU-bound behavior, real momentum scrolling, confirming a fix in the
   reporter's hands). This step was skipped in the ADR 0076 investigation at a
   cost of nine record-and-analyze cycles.
1. Ask the reporter to enable **Settings → Performance instrumentation**, use
   the app normally, and send a **screen recording** of the problem.
2. Read the overlay out of the video (`ffmpeg` to crop and upscale the readout
   region, then read the frames).
3. Measure the behavior independently from the same video — frame extraction
   plus 2D phase correlation gives per-frame displacement. ADR 0076 documents
   the two traps: 1D profiles alias against the row pitch, and only near-still
   frames yield meaningful displacements.
4. Correlate the two. Marks carry page-relative timestamps and frames carry
   video-relative ones; align on a distinctive event.

The loop's value is that it **falsifies** hypotheses instead of accumulating
them. Its three most useful moments so far were all negative results: proving a
deployed bundle predated the fix under test, proving iOS was not rejecting
`scrollTop` writes, and proving a correction had never run at all.

## Consequences

- Developer tooling now ships in the production bundle. It is gated and inert
  when off (one boolean check per mark), but if a release build should exclude
  it, that is a build-flag decision to make deliberately rather than by default.
- The overlay's mark list is curated by prefix and will need extending as new
  instrumentation is added. It is a ten-line buffer: adding a chatty mark
  crowds out everything else, which is a real failure mode (a chain re-armed
  every scroll frame once flooded it and hid the marks being investigated).
- The summary only covers the back-to-room-list transition. Other transitions
  would each need their own reduction; the pairing helpers are reusable.
- **When adding instrumentation, ask whether a phone user could read it.** A
  mark only a desktop console can reach does not help with the reports that
  most need help.
