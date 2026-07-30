# ADR 0081 — Web client multi-image send, galleries, and lightbox paging

## Context

The web client handles exactly one image at a time. `useAttachment`
(`clients/web/src/media/use-attachment.ts`) holds a single staged `File` and a
single preview object URL; the composer's file input takes one file and its
paste handler reads `clipboardData.files[0]`; `sendMedia` in
`clients/web/src/stores/timeline.ts` pushes one echo and awaits one upload; and
every image renders as its own full-width `MessageEventRow`. `Lightbox.tsx`
shows a single photo with no way out but back to the timeline.

The practical effect is that sending five photos means five separate trips
through the picker, and reading them means five full-width rows and five
open/close cycles. This ADR closes all three gaps together, because they are
the same gap seen from the send, render, and view sides.

### There is no album, and there cannot be one

Matrix has no album primitive — no event type, no relation, that says "these
N images were sent as one gesture". Clients that appear to group images
(Element's gallery view) infer it from adjacency, exactly as proposed below.

More immediately, **Axon cannot round-trip custom event content even if the
spec had one**. `SendMessageRequest` and `SendMediaRequest`
(`crates/axon-api/src/dto.rs:402,427`) are fixed structs with no
`#[serde(flatten)]`, and `SdkGateway::send_message` / `send_media`
(`crates/axon-sync/src/gateway.rs`) build the outgoing content literally, from
a closed set of known fields. There is no field in which a client could put an
album marker, and no path by which the marker would survive to the timeline
API, which has no msgtype filter either. Generic passthrough is issue #130 and
is explicitly out of scope here.

So a multi-image send is **N independent `m.image` events carrying no marker
of their common origin**, and grouping must be **inferred at render time**. The
entire feature is client-side and lives in the web silo.

### What the live data says

A read-only scan of the development account (~650 events across 10 rooms,
2026-07-26) turned up 13 image events and three facts that shaped the design:

- **Sender thumbnails are rare: 2 of 13 (15%)**, and 12 of 13 events are
  encrypted. That combination is the expensive one.
  `resolve_thumbnail_spec` (`crates/axon-api/src/routes/media.rs:308`) rejects
  encrypted media with a 400, unconditionally and with no fallback, and
  `MediaImage.tsx:48-50` correspondingly only asks for a server thumbnail when
  `!media.encrypted`. In an E2EE room a **sender-embedded thumbnail is the only
  kind that can exist** — so 85% of the time a gallery cell would have nothing
  to show but the full-size image. Those run to a median of 65 KB, a mean of
  318 KB, and a maximum of 1.16 MB; a naive 12-cell grid averages ~3.7 MB.
- **5 of 13 events carry no `w`/`h` at all**, so today's per-image scroll-space
  reservation already degrades silently for a large minority of images.
- **Zero natural multi-image runs exist** in the account. Grouping cannot be
  validated against history; its correctness rests entirely on unit tests and
  seeded end-to-end fixtures.

The missing dimensions and thumbnails are Axon's own doing — `attachment_info`
(`crates/axon-sync/src/gateway.rs:396-407`) builds `BaseImageInfo` with `size`
and defaults everything else — and fixing that is issue #384. That fix is
write-side only: it cannot improve images already in history and only helps
images sent *by* Axon clients, so it reduces but does not remove the need for
the size gate below. It is an API-silo change and does not block this work.

### What the clipboard actually delivers

Copying two images from a web page and pasting yields
`clipboardData.files.length === 0` — only `text/plain` and `text/html`, no
files whatsoever (measured in Chromium). Whether an OS file-manager copy
delivers more than one file is untested. This turns out not to matter: the
composer hands `clipboardData.files` to the staging hook wholesale and stages
whatever arrives, N or 1, with no special-casing. The picker and drag-and-drop
give genuine multi-select regardless, so the feature is complete either way.

## Decision

### Grouping is inferred from adjacency, in a pure function

A new `clients/web/src/timeline/group-media-runs.ts` maps the event array to a
row array before rendering:

```ts
export type TimelineRow =
  | { kind: 'event'; key: string; event: TimelineEvent }
  | { kind: 'gallery'; key: string; events: TimelineEvent[] }
```

An event joins the current run only if it is image or sticker media, from the
same sender, within `GALLERY_WINDOW_MS` (60 s) of the previous one, on the same
day, and is not redacted, not a state event, not a reply, not edited, not
reacted to, and not a failed echo. Runs shorter than `GALLERY_MIN` (2) flush as
ordinary rows; runs longer than `GALLERY_MAX` (12) split. Because the walk is
linear and any failed predicate flushes the run, "no intervening events of any
kind" falls out for free rather than needing its own check.

It is a pure function over the array, not a component and not store state, so
it is trivially testable and imposes no ordering constraints on the store.

**Read receipts deliberately do not break a run.** They change on every
ephemeral update, and re-grouping on them would relayout the timeline with no
user action behind it — precisely the hazard ADR 0076 exists to prevent. The
run renders its last event's receipts beneath the grid.

**Reactions, redactions and replies do break a run**, which means reacting to
an image in the middle of a gallery splits one row into three, live. This is
accepted: it is rare, user-initiated, and absorbed by the existing
scroll-anchor correction. The alternative — rendering reaction chips per cell
— would drag `MessageEventRow`'s entire chip and picker apparatus into the
grid.

**An edit does not.** It was originally listed with the others, but the
analogy does not hold: a reaction adds chips and a picker, whereas editing an
image's caption changes *content the grid already shows beneath the cells*.
Losing a whole gallery because one caption was corrected is a jarring price
for an "(edited)" marker — reported in testing as the gallery falling apart
for no visible reason. `origin_ts` is unaffected by an edit (the edit time
lives in `latest_edit_ts`), so the run's timing does not move either. The cost
is that the marker is not visible in grid form; the expander still reaches it.

Captions therefore render **beneath the grid**, not on the cells: a square
thumbnail has no room for text, and an overlay hides the picture the reader
opened the gallery to look at. They are numbered when a run carries more than
one, so a caption can be tied to its image.

### A gallery is one `<li>`, because scroll anchoring requires it

`captureAnchor` (`clients/web/src/pages/RoomPage.tsx:1352-1408`) binary-searches
`li.event-row` elements on the assumption that document order is monotonic in
vertical position. A gallery must therefore be a single
`<li class="event-row gallery-row">` in the same `<ol>`, carrying the first
event's id in `data-event-id`, or the search breaks for every row below it.

Cells use `aspect-ratio: 1` with `object-fit: cover` in a **fixed**
`repeat(3, 1fr)` grid — not `auto-fit` — so a run's height is `ceil(n / 3)`
rows regardless of container width. Given that 5 of 13 images carry no
dimensions, this is *better* than the status quo: gallery rows reserve their
space correctly even for images whose size is unknown, where a single image row
cannot.

**Highlight-and-jump highlights the cell, not the run.** Forcing the jump
target back to an ordinary row via the `breaks` predicate was tried first and
is wrong: a deep link that points *into* a gallery then tears it into up to
three rows, so following a link to one image destroys the grouping around it.

Instead every cell carries its own `data-event-id` and takes `.highlighted`
when it is the target. `centerHighlightedRow` finds and centres the cell,
because it scans `[data-event-id]`. Scroll anchoring is untouched:
`captureAnchor` searches `li.event-row`, and a cell is an `li.gallery-cell` —
the two selectors were already distinct, which is what makes this safe.

### A gallery has a byte budget, rather than a cap on its size

An earlier draft capped `GALLERY_MAX` lower for encrypted rooms. That is the
wrong lever: 12 images split across three rows of four still fetch 12
full-size objects. Splitting rows does not move bytes.

The first attempt at the right lever was a **per-image** ceiling of 256 KB,
calibrated from the probe above — 13 images, 65 KB median. That sample was
mostly screenshots and pasted images. Against real camera photos, which run
1–3 MB each, it deferred *every* cell and turned a gallery into a wall of grey
tiles. The gate meant to catch the exceptional case was catching the normal
one, and a gallery that shows nothing is worse than one that takes a moment.

So the budget belongs to the **run**, not the image:

```ts
export const GALLERY_EAGER_BYTES = 8 * 1024 * 1024
```

Cells load in order until the running total passes it; the rest render a
click-to-load tile. That makes the decision deterministic by position — the
first cells always fill in, and a four-photo post lands well inside the
budget — and it is decided by the row, the only place a run's cumulative size
is known. A cell can see its own size and nothing else, which is precisely why
the per-image rule could not distinguish "one big photo" from "twelve of them".

Two details worth keeping:

- **An unknown `info.size` spends the full budget**, not zero. Unknown is
  exactly the case that might be enormous.
- **Media the server or sender already thumbnailed costs nothing.** Only an
  encrypted image with no sender thumbnail has to pull its full size, because
  `resolve_thumbnail_spec` refuses to thumbnail encrypted media at all.

What the budget is *not* protecting against is unbounded parallelism: cells
already load lazily through an `IntersectionObserver`, and `media-service`
caps concurrency. The concern is total bytes pulled to draw one row on a
connection where that matters.

### One lightbox per surface, with a cursor that tracks identity

A new `MediaViewerProvider` renders a **single** lightbox for a whole timeline
rather than one per row — which is what makes paging possible at all. The
timeline subtree and `ThreadPanel` are wrapped separately, since a thread's
images are their own sequence.

The sequence is built from `RoomPage`'s `visible` array, not the raw store
slice. The slice contains thread replies and hidden state events the timeline
refuses to render; paging into one would strand the user on an image with no
row to return to on close.

**Cursor state is `{ eventId, ts }`, never an index.** The index is recomputed
each render, which makes back-pagination prepends and live appends free. When
the id disappears — redaction, gap-fill — the viewer falls back to the
nearest-newer event by timestamp, then to the last, then closes.

`MediaImage` gains an optional `eventId` and opens the shared viewer when a
provider is present. **With no provider it behaves exactly as it does today**,
which keeps `MediaPreview`, search results, and the existing tests untouched.

### Paging is asymmetric: history loads, the future does not

Stepping older from the first image sets a pending intent that an effect acts
on, calling the timeline's existing older-page loader and re-checking. The loop
must live in an effect rather than the click handler because the event list
arrives as a prop. It chains, because a 50-event page can easily contain no
images at all, and is bounded at `AUTO_PAGE_LIMIT = 5` pages, mirroring the
existing `AUTO_SCROLL_BACK_PAGES`. The newest end is simply inert.

Exactly ±1 neighbour is preloaded. `LRU_CAP = 32` in `media-service.ts` counts
*entries, not bytes*, so three simultaneously-live full-size photos is already
~12 MB; ±3 would be ~28 MB and noticeable on a phone. Backtracking is fast
regardless, because released blobs park in the zero-reference LRU. Preloading
is skipped under `navigator.connection?.saveData`.

Swipe paging binds to the lightbox overlay. There is no collision with the
room's swipe-to-back gesture: those handlers are on `.room-body` while the
lightbox portals to `document.body`, and Preact portals do not re-dispatch DOM
events through the virtual tree. This is asserted by an end-to-end test rather
than defended with guards. ADR 0075's tuned gesture constants — including its
30 px native-back edge band, which still applies because WebKit's edge
recognizer is live over a fullscreen overlay — move to a shared module so the
room and the lightbox cannot drift apart.

### Saving a displayed image

Outside this ADR's three parts, but it changes the lightbox chrome defined
here, so it is recorded with them rather than in a fourth place.

**The transient anchor is the only sanctioned path.** `window.open` is banned
repo-wide for the Tauri shell (M-W12), which rules out the usual mobile trick
of opening the object in a new tab and letting the user save it from there.
`MediaAttachment` already owns the working pattern — `fetchBlobUrl`, an anchor
appended, clicked and removed, then `revokeObjectURL` on a 60-second delay so
the browser has time to start writing — and that moves to a shared helper
rather than being copied. The delayed revoke in particular is the kind of
detail that rots silently in a second copy.

**It re-fetches rather than reusing the displayed blob.** The viewer is already
holding an object URL for the full-size image, and reusing it looks free, but
that URL is owned by `useMediaBlob`'s reference counting and can be evicted by
the `LRU_CAP` cache while the browser is still writing the file. `fetchBlobUrl`
hands back a separately-owned URL the caller revokes on its own schedule, and
it is normally served from cache anyway. Ownership decides it, not cost.

**It is offered only for an image that actually decoded.** The media proxy
returns **200 with raw ciphertext** when it lacks the decryption key, so a
ready blob is not necessarily an image. `MediaImage` catches that at `<img>`
decode and says so; `LightboxImage` did not, and rendered a broken-image icon
with no explanation. It now tracks the same `decodeFailed` state — a fix worth
making on its own, and a precondition here, because otherwise the button would
offer to save undecryptable bytes under a plausible `.jpg` name.

**Mobile gets the share sheet.** On iOS an `<a download>` saves to Files, not
Photos, which reads as the download having silently failed. Where
`navigator.canShare({ files })` reports support, the button hands the file to
`navigator.share` instead, so the native sheet offers "Save Image" and
AirDrop; everywhere else — desktop, and the Tauri shell — it falls back to the
anchor. Capability-detected rather than user-agent sniffed, and the fallback
path is the one that must keep working.

### Paging spans the timeline; only the label knows about runs

The viewer pages across **every** image in the loaded timeline, not just the
ones in the gallery a cell was opened from. Earlier drafts recorded this as a
bare decision without arguing it, and without considering how it would read
once galleries existed — which is worth stating plainly, because the obvious
alternative (open a cell, page within that post, stop at its edges) is what an
album mental model suggests.

The deciding argument is that **grouping is a heuristic with known false
positives**. A bridge stamping identical timestamps, or a bot posting rapidly
as one user, will group images that were never one gesture; that is accepted
below. The two designs fail very differently under that error:

- Paging globally, a mis-grouped run is a cosmetic row-layout mistake.
- Paging *within* a run, a mis-grouped run becomes a navigation cage — the
  user is either trapped among unrelated images or cut off from images that
  genuinely belong together, with no way out but closing and reopening.

Scoping navigation to an inferred boundary makes the heuristic load-bearing,
and it fails badly rather than gracefully. Messaging clients with the same
weak-album problem land in the same place: Signal and WhatsApp both page
across all conversation media. iOS Photos scopes to an album, but its albums
are real rather than inferred.

What the album model does get right is that `3 of 47` is meaningless to
someone who just opened a five-image post. So the counter reports **only the
position within a gallery** — a compact `2/5` — and shows nothing at all for
an image that is not in one, leaving just the caption where there is one.

An index among every loaded image was tried and dropped. It is not merely
useless: it *moves on its own*, because back-paginating pulls in unrelated
older images and the total climbs while nothing has been sent. A five-image
post reading "4 of 8" was reported in testing as images having appeared from
nowhere. `groupMediaRuns` is computed for the timeline anyway, so the run
lookup costs the viewer nothing beyond a scan.

The status element stays in the DOM even when it has nothing to say — a live
region created at the moment its text changes is unreliably announced — and
collapses with `:empty` rather than painting an empty pill. Pagination
progress and the oldest-end boundary still announce through it.

Nothing blocks at a run boundary — stepping past the fifth image continues
into the timeline, and only the label changes.

### Sending is sequential, and the first image carries the caption

`useAttachment` becomes `useAttachments`, holding an ordered list whose items
have stable ids (a `File` is not a usable key — two identical pastes collide).
Staging is **additive**: pick three, paste a fourth, remove one. The hook keeps
its existing ownership discipline exactly — it owns every preview object URL
and revokes on remove, clear, scope change, and unmount — now over a map rather
than a single ref. Caps are `MAX_BATCH_FILES = 10` and the existing
`MAX_UPLOAD_BYTES`, applied to the accumulated total.

The store gains `sendMediaBatch`, which pushes **all** local echoes
synchronously and in order, so the gallery appears complete the instant the
user hits send, then uploads **sequentially**. Sequential is not a
simplification: it is the only thing that preserves room order, since the
server assigns ordering at send time. Each upload failing is independent; the
loop continues, and `sendMedia` is retained as a one-element call into the same
path so every existing caller is unaffected.

**Relations are deliberately asymmetric.** The composer's text captions the
**first** image only, as does `reply_to`; `thread_root` applies to **all** of
them. Thread membership is a destination, not a per-event decoration, whereas N
reply-context blocks would both look wrong and break the gallery run, rendering
a replied-to batch as N full rows.

### The expander keeps every per-event affordance

A gallery row can expand into N ordinary `MessageEventRow`s. This is about
fifteen lines and no duplicated logic, and it keeps reply, edit, delete, react,
thread, inspect, and retry reachable without the gallery reimplementing any of
them. Without it the gallery would either lose those affordances or grow into a
second copy of `MessageEventRow`.

### Accessibility

Three things this feature can get wrong, roughly in order of severity.

**Focus restoration must follow the cursor, not the entry point.** This is the
one that needs a change to shared code. `useModalFocus`
(`clients/web/src/components/use-modal-focus.ts`) is the app's modal contract
(WCR-14): it captures the previously-focused element once, in an effect with
empty dependencies, and restores it on unmount. That is exactly right for a
lightbox showing one image and exactly wrong for one that pages — open image 1,
page to image 7, press Escape, and focus lands back on cell 1, silently
rewinding the user six images.

Back-pagination makes it worse: it can detach the captured node, and
`previouslyFocused.focus()` on an element no longer in the document is a no-op
that leaves focus on `<body>`, dropping the user at the top of the page.

So `useModalFocus` gains an optional `restoreTo?: () => HTMLElement | null`,
resolved **at unmount** rather than captured at mount. The viewer points it at
the current cursor's cell via `[data-event-id]`, falling back to the gallery
row and then the timeline scroller — never a detached node, never nothing.
Callers that pass nothing keep today's behaviour exactly, so this stays one
contract rather than becoming two.

**Paging is keyboard-first, and its state is announced.** Arrow keys page, and
prev/next are real `<button>`s. The inert newest end uses the `disabled`
attribute, not `aria-disabled` alone: `collectFocusable` filters on
`button:not([disabled])`, so an aria-only version would leave a dead stop
inside the focus trap.

Focus deliberately does **not** move when the image changes — it stays on
whatever control the user is operating. The change is announced instead by a
single `aria-live="polite"` status region carrying position, pagination
progress, and boundaries: "3 of 12", "Loading older messages", "Oldest image".
Updating the dialog's `aria-label` per image is not a substitute; a live region
is the only thing reliably announced on an already-mounted dialog. The
`AUTO_PAGE_LIMIT` bound in particular needs an audible end state, because
stopping silently is indistinguishable from a hung page.

**Immersive mode has a keyboard contract.** A tap on the image hides the
overlay chrome so the photo can be seen unobstructed; another tap restores it.
The chrome is hidden with `opacity` and `pointer-events`, deliberately *not*
`display: none` or the `hidden` attribute — `collectFocusable` skips hidden
elements, and emptying the focus trap makes it early-return, which would let
Tab escape the dialog entirely.

Two consequences fall out. Entering the mode parks focus on the dialog itself
(`tabindex="-1"`), because the mount focus sits on ✕ and leaving it there would
mean an invisible button still holds focus — Enter or Space would then close
the viewer with nothing on screen to explain why. And `focusin` on the
container restores the chrome, so tabbing to any control reveals it the moment
it takes focus. A keyboard user therefore cannot end up driving controls they
cannot see.

The controls also need to outrank `button.ghost`, whose `background:
transparent` is more specific than a bare `.lightbox-page` class and silently
won — which is why the paging arrows (and, it turned out, the pre-existing ✕)
rendered nearly invisible against a photo. `disabled` is styled by colour
rather than opacity, so it cannot fight the hidden state.

**A gallery must not become a wall of unlabelled buttons.**

- The grid is a real `<ul>` of `<li>` nested inside the timeline row's `<li>`
  (valid HTML), so assistive technology gets "list, 7 items" with no ARIA at
  all, and the list carries a label naming sender and count.
- Each cell button's accessible name carries its position — "Image 3 of 7,
  cat.png" — so a cell is never announced as a bare "button".
- A size-deferred cell is named for what it does ("Load image, cat.png,
  1.2 MB") and never impersonates a loaded image.
- Cells use a **roving tabindex**: one tab stop per gallery, arrow keys moving
  between cells, reusing the grid-navigation idiom already in
  `MessageEventRow` for the reaction picker (which derives its column count
  from `gridTemplateColumns` — a gallery's fixed three columns are simpler
  still). Twelve cells across several galleries would otherwise make a
  photo-heavy room effectively untabbable.
- The expander is a labelled toggle carrying `aria-expanded`. Collapsing moves
  focus to the expander itself rather than letting it die with the rows it
  removes.

**Alt text is `caption ?? filename`, and that is the ceiling.** Matrix carries
no alt-text field, and `body` is just the filename echoed back when the sender
set no caption. A batch of `IMG_2831.JPG` therefore yields twelve nearly
identical names. That is honest — it is precisely what the sender sent — but it
is not good, and no client-side change can fix it. Stated here so it is not
rediscovered later as a bug.

In the composer (PR 4), each staged thumbnail gets a named remove button
("Remove cat.png"), the "captions the first image" hint is associated with the
input through `aria-describedby` rather than being visual-only, and staging and
removal are announced through the same polite-status pattern.

## Consequences

- **Retrying a failed image loses its position.** Retrying image 3 of 5 lands
  it after 4 and 5, permanently, because the server assigns order and there is
  no marker with which to restore it (#130). Users may read this as shuffling.
- **A reaction, edit, or redaction on a middle image splits the row, live.**
  There is no clean fix without an album marker; the expander is a partial
  answer.
- **Deferred cells show a tile, not a picture.** Large encrypted images without
  sender thumbnails wait for a click. Measured at roughly 3 of 11 such images.
  If it reads as broken rather than deliberate, `GALLERY_EAGER_BYTES` is the
  dial; #384 shrinks the affected population for future Axon-sent images. The
  first calibration of this got it badly wrong — see the byte-budget section —
  so treat any number here as provisional until it has met real photos.
- **Lightbox paging silently grows the retained timeline.** `trimRetainedTail`
  only trims the newest end, so paging back through 30 images can load five
  pages that stay in the DOM (#26 — the timeline is not windowed), raising the
  ADR 0071 teardown cost from what the user experienced as looking at photos.
  `AUTO_PAGE_LIMIT = 5` is the mitigation.
- **The iOS edge-swipe ambiguity is inherited, not solved.** Swipe-right-to-
  older and WebKit's back gesture differ only in start position. ADR 0075
  already litigated this and declined to reserve the edge band; the lightbox
  takes the same trade for consistency.
- **Heuristic grouping will sometimes be wrong** — a bridge stamping identical
  timestamps, or a bot posting rapidly as a single user, will group images that
  were never one gesture. The expander gives a way out, and paging deliberately
  does not depend on the boundary being right.
- **The run-aware counter inherits that fallibility.** A wrong grouping will
  sometimes claim "2 of 5 in this post" about images that were not one post.
  That is a mislabelled count rather than a navigation dead end, which is the
  trade this ADR chose on purpose.
- **Grouping has no historical corpus to validate against**, so its
  correctness rests on unit tests and seeded fixtures rather than on
  observation of real traffic.
- **Multi-image paste may never fire.** A web-page copy provides no files at
  all, and the file-manager case is untested. The picker and drop paths are
  unaffected, and the implementation needs no branch either way.
- **`useModalFocus` grows a second responsibility.** Adding `restoreTo` widens
  a contract every overlay depends on. It is opt-in and the default path is
  unchanged, but it is shared code touched for one caller's benefit, and a
  regression there would affect every modal — so PR 2 pins the existing
  restore behaviour with a test before adding the option.
- **Gallery cells navigate differently from the rest of the timeline.** A
  roving tabindex means Tab leaves a gallery in one press while arrows move
  inside it, which is correct per APG and consistent with the reaction picker,
  but it is a second keyboard model in the timeline and will need to be
  documented for users alongside the existing shortcuts.
- **Screen-reader users get no real image descriptions**, because Matrix
  provides nowhere to put them. Galleries make this more visible by putting
  many weakly-named images in one row.
