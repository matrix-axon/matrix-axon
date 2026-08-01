# ADR 0086 — Demo corpus and client recording

## Context

Axon has no visual documentation. `README.md` describes an architecture and
lists two active clients, but shows neither. Capabilities that took whole
milestones to build — Tantivy full-text search (M9), threads (ADR 0032), the
Spaces picker (ADR 0084), adjacency-inferred image galleries with lightbox
paging (ADR 0081) — are invisible to anyone evaluating the project.

The only content anyone has ever seen a client render is the **Axon Testing**
room (`!SScJmZuEkBUnuydXdf:bostoncoop.net`): a real room on a real remote
homeserver, full of manually typed dummy messages, that nothing in this repo
creates or controls. It exists because `clients/web/AGENTS.md` confines live
mutations to it. It is the right tool for that job and the wrong one for a
screenshot — its contents are arbitrary, unreproducible, and shaped by whoever
last tested something.

Separately, the project's fake-data story is split three ways and none of the
three can produce a realistic world:

- `clients/web/e2e/mock-server.mjs` has the richest *shapes* (spaces, gallery
  runs, search, a real WebSocket) but is JavaScript fixtures serving one client,
  never a homeserver.
- `smoke/local-stack` boots a real Synapse + Postgres + axon and can already
  backdate events through an application service, but every message body is a
  literal like `"jump fixture 2026-01-03 message 07"`, and it can create no
  media, no spaces, and no DMs.
- `crates/axon-itest` seeds one encrypted room to prove one re-decryption path.

So the gap that blocks a demo video is the same gap that blocks several classes
of smoke test. `smoke/tui/README.md` already records the consequence: media
rendering "is not covered; the stack needs uploaded media fixtures."

## Decision

### One corpus, rendered into the real stack

A single declarative corpus is the source of truth for demo content, and
`smoke/local-stack` is its interpreter. Both clients then read that content
through a live axon over `/v1/`.

The rejected alternative was to also teach `mock-server.mjs` the same corpus so
the web client could be recorded without Docker. That would put one data file in
service of two silos, and the root `AGENTS.md` one-silo-per-PR rule makes a
shared fixture straddling `smoke/` and `clients/` a recurring source of
awkward PRs. Going through the real stack costs a Docker dependency and buys
correctness for free: search results come from a real Tantivy index rather than
a mock's substring match, media really traverses the proxy and its LRU cache,
and the WebSocket is the real one. `mock-server.mjs` is left exactly as it is,
serving the PR-gating e2e lane it was built for.

### The corpus is data; the seeder is code

`smoke/local-stack/corpus/demo.toml` declares personas, spaces, rooms, and a
flat list of messages carrying relations, reactions, and image references.
Timestamps are **relative** (`at = "-6d 09:12"`) so a recording made at any time
shows a timeline that reads as current, and `/jump`, day separators, and search
date filters all have real spread to work against.

Keeping it declarative — rather than another `seed_*` function — is what lets
the corpus grow without a Rust change each time, and lets a scenario author
encode an awkward case (a gallery run 61 seconds apart, a day boundary crossed)
as three lines of data.

### Backdating keeps using the application-service route

`seed_long_timeline` already backdates via `?user_id=&ts=` through a registered
appservice. That mechanism is kept and generalized: `send_appservice_message`
hardcodes `m.text` and a bare body today, so it splits into a
`send_appservice_event(..., event_type, content, ts)` that can backdate *any*
event — which is what makes historical images, threads, and formatted messages
possible at all. The narrow wrapper stays so `seed_long_timeline` is unchanged.

The appservice registration declares a **non-exclusive wildcard user
namespace**, so personas get clean IDs (`@maya:localhost`) instead of
namespace-prefixed ones. This is a deliberate test-only choice: the registration
file is generated per run into a temp dir for a disposable local homeserver, and
a demo where every participant is `@rtc_maya:localhost` undercuts the point of
the exercise.

### The demo corpus is opt-in

`up` gains `--corpus <path>`, defaulting to unset. Every existing smoke scenario
and every existing manifest field behaves exactly as before. The demo world is
additive, so the smoke gate remains the regression check for this ADR's changes.

### The TUI pilot is a PTY passthrough, and declares its image protocol

axon-tui renders real Sixel/Kitty/iTerm2 graphics, which is genuinely
distinctive and which no headless recorder reproduces: `agg` does not render
Sixel, and xterm.js-based recorders like VHS do not either. The recording
therefore happens in a real terminal on a developer's machine, with a human
running the screen recorder.

To drive it without owning the screen, `axon-demo-tui` opens a PTY, spawns
`axon-tui`, and pumps PTY output to stdout **verbatim** — graphics escape
sequences are just bytes and survive the copy. It reuses `PtyDriver` from
`smoke/tui/src/pty.rs`, which requires adding a `lib.rs` to that package so the
type is importable rather than duplicated.

`axon-demo-tui` ships as a **second binary of the `axon-smoke-tui` package**
rather than as its own crate. A `smoke/demo-tui` package could only reach
`PtyDriver` through a path dependency on `axon-smoke-tui`, and
`scripts/check-smoke-isolation.sh` rejects every `axon-*` edge but the package's
own — so the alternative was weakening the isolation check to land a demo tool.
The binary keeps the name; only its package differs.

Critically, the pilot **sets `AXON_IMAGE_PROTOCOL` and `AXON_FONT_SIZE`
explicitly and does not set `AXON_NO_IMAGE_QUERY=1`.** A pilot-owned PTY will
not answer the TUI's DA1 capability probe (`main.rs:205`), so the protocol must
be declared rather than detected. The smoke harness sets `AXON_NO_IMAGE_QUERY=1`
for the opposite reason — it wants determinism and does not care about images.

Steps wait on screen predicates via the existing `vt100` model rather than
sleeping, so a recording does not desync on a slow machine, and the script ends
by typing `/quit` so the alt screen unwinds cleanly — `smoke/tui/src/runner.rs`
kills the child instead, which would end a recording on a corrupted terminal.

### The web recording is automated

Playwright captures video natively, deterministically, and at a fixed size, and
`clients/web/` has no screenshot or video configuration today to conflict with.
A separate `demo` project is added rather than reusing the `chromium` project,
so `pnpm test:e2e` and the PR gate are untouched.

**Desktop and mobile are both recorded**, as two Playwright projects —
`demo-desktop` at a fixed desktop viewport and `demo-mobile` using a device
descriptor, which is how mobile earns real device pixel ratio, touch, and a
phone-shaped viewport rather than a narrow desktop window. Mobile is not a
resize of the desktop take: the layout collapses the room list and the spaces
rail behind navigation, so the scenes differ in kind — a desktop scene points at
a room that is already on screen, a mobile scene has to navigate to it, and
the room-switch cost that ADR 0070 measures on an iPhone 13 is only visible in
the mobile recording. `docs/demo-coverage.md` therefore tracks web coverage per
form factor, not once for "web".

Nothing about the corpus or the seeder is viewport-aware — both recordings read
the same seeded world through the same axon — so the form factors are purely a
recording-side concern.

Two things the TUI pilot learned the hard way, which apply to any driver and are
worth not re-learning:

- **A step that changes state needs an assertion only the new state satisfies.**
  Four of the TUI scenes first "passed" against the *previous* frame, which let
  the script run ahead of the client so the next input landed somewhere
  unintended. Playwright's auto-waiting removes the sleep problem but not this
  one: prefer state-unique chrome over content that is on screen either way, and
  assert on what *left* (`toBeHidden`) to prove a narrowing step narrowed.
- **A scene that mutates should script the undo**, so it leaves the world as it
  found it. Otherwise its own second run is satisfied by what its first run left
  behind, and the assertion quietly stops meaning anything.

Both are written up with examples in `smoke/tui/README.md` ("Writing a scene"),
and `docs/demo-coverage.md` ships with its web columns empty and its rows
already listed, so the phase-3 gap is visible rather than discovered later.

Two presentation details are load-bearing and easy to miss: Playwright renders
no mouse cursor into video, so the demo injects a cursor overlay or the UI
appears to operate itself; and `axon.settings` defaults `spacesPaneAutoHide` to
`true`, which will hide the spaces rail mid-scene unless the demo seeds it
`false`.

### Videos are published, not committed

`.git` is 3.8 MB with no LFS. Video is attached to a GitHub release and pulled
into the Pages artifact by `.github/workflows/api-docs.yml` at build time; only
small poster stills are committed. GitHub renders repo-relative GIFs inline in a
README but not repo-relative MP4/WebM, so the README carries posters linking to
the Pages-hosted video rather than inline motion.

**As built** (the TUI recording, PR #109). The shape below is what a second
recording — the web ones from the phase below — should slot into rather than
reinvent:

- Assets hang off one **pre-release**, `demo-2026-08`, named by a single
  `DEMO_RELEASE_TAG` env var in the workflow. Pre-release is deliberate:
  `--latest=false` cannot demote it while the only other release (`v0.0.1`) is
  itself a pre-release, and a video labelled "Latest" reads as the project's
  current version.
- The download step pulls **`*.mp4`**, so **more recordings added to the same
  release need no workflow change** — only a `test -s` guard line and a player
  on the page. A *new* release tag would mean bumping `DEMO_RELEASE_TAG`.
- `demo.html` at the repo root is the player page, served at `/demo.html`. It
  carries **no Jekyll front matter on purpose**: Jekyll converts files that have
  it and copies everything else verbatim, so the page keeps its own markup
  instead of inheriting the theme-less rendering the homepage gets from
  `README.md`.
- A `<video>` tag **cannot** live in a README. GitHub sanitises HTML in markdown
  and strips it; Jekyll, rendering that same README into the Pages homepage,
  does not. The tag would therefore appear to work on the site while silently
  showing nothing on github.com. Posters linked to `/demo.html` render
  identically in both.
- Poster stills are palette-reduced PNG, ~126 KB at 1200px wide. PNG rather than
  JPEG because JPEG rings around terminal text; this is the only piece that
  enters git history, so it gets the same scrutiny as the corpus photographs.
- The workflow guards that `_site/demo.html` survived the Jekyll build and that
  the video was actually downloaded, joining the two guards already there. A
  demo page with a broken player is the same silent-success failure that
  workflow's comments already record twice.

### Demo coverage is tracked like client parity

`docs/demo-coverage.md` records, per visually significant capability, which TUI
and web demo scene covers it — maintained under the same same-PR rule as
`docs/client-parity.md`, which exists because exactly this kind of cross-silo
status silently drifted before. A feature that never reaches a demo script is
invisible twice: absent from the videos, and unexercised by the driver.

### Bug-catching capability is harvested, not assumed

A fixture is scenery until something asserts it. The corpus is written so that
its awkward cases are *assertable*, and turning them into assertions is the
point of the exercise rather than a by-product of it — the recordings are what
this work is visible as, but the tests are what it is worth.

The clearest case is already in `demo.toml`: a four-image run seconds apart from
one sender, then a fifth image four minutes later with a text message between
it and the run. That pair exists to pin ADR 0081's adjacency heuristic, which
`docs/client-parity.md` flags as a heuristic a bridge stamping identical
timestamps could defeat. The corpus supplies both the positive and the negative
case; nothing yet checks either.

This is a different axis from `docs/demo-coverage.md`. That table tracks whether
a **scene shows** a capability; this tracks whether a **test asserts** it. A row
can name a scene and still be asserted by nothing — `smoke/tui/README.md`
records exactly that for media rendering, which the pilot renders for real while
only a human eye confirms it.

Four harvests follow directly from what the seeder now makes possible: gallery
adjacency including the near-miss (`clients/web`), TUI media rendering
(`smoke/`), DM title derivation (both, since web ported the rule from
`clients/tui/src/app/rooms.rs` and the two can drift), and backdated history and
jump-to-date against the corpus's multi-week span (`smoke/`). The silos differ,
so this is necessarily more than one PR.

The standing question for any PR that extends the corpus: *what can we now
assert that we could not before?* Answer it in the PR body.

Deferred, tracked in #111.

### The demo stack is reachable from local development

The corpus is also the most realistic environment a developer can get without
touching the real Axon Testing room, so it should be reachable from ordinary
development rather than only from a recording session. Three tiers, cheapest
first:

1. **Agent-run, every change.** The per-silo commands stay the fast path and are
   unchanged.
2. **Pre-push, opt-in.** A `demo` target in `scripts/smoke-gate.sh`, so the
   corpus lane is reached through the `RUN_SMOKE=<lane>` hook `.githooks/pre-push`
   already has rather than a second mechanism. Opt-in because it needs Docker
   and takes minutes.
3. **Human-confirmed.** Real Sixel/Kitty rendering, gallery layout, and lightbox
   paging have no honest headless substitute, so an agent changing visual
   behaviour offers the one-command check and asks for an eyeball. This is the
   role `clients/web/AGENTS.md`'s "human pass against the live server" plays
   today, moved onto a disposable stack.

`scripts/demo-stack.sh` stays what its header says it is — not a test and not a
gate. What is missing is the gate-side target, not a second entry point.

Deferred, tracked in #111.

## Consequences

- The demo and seeding lane is **Unix-only**. `smoke/local-stack` guards its
  `setsid()` process-group detachment behind `#[cfg(unix)]`, and `smoke.yml` and
  `integration.yml` run `ubuntu-latest` only. Windows contributors keep
  `cross-build.yml`, axon-tui, and axon-web; they cannot run this lane. This is
  inherited scope, not a new restriction.
- Recording requires Docker and a real terminal, so it cannot run on a
  GitHub-hosted runner end to end and is not a CI gate.
- A `--corpus` stack is meant to stay up while a recording is made, which makes
  it the first stack expected to coexist with an ordinary smoke run. The local
  stack therefore pins every on-disk path (SDK store, search index, media cache
  and uploads) under its own run directory; the defaults are shared platform
  directories, and Tantivy's exclusive index lock turns that sharing into a
  second stack that will not boot.
- **Publishing a new take is a manual workflow run.** Re-uploading an asset to
  the release changes no path in the repository, so nothing in the `paths:`
  trigger fires; `api-docs.yml` has to be dispatched by hand afterwards or the
  site keeps serving the previous recording. Actions-based Pages also replaces
  the entire site per deploy, which is why the download runs on every build
  rather than incrementally.
- Videos are regenerated by hand, so they can go stale. `docs/demo-coverage.md`
  plus definition-of-done entries in both client `AGENTS.md` files are the
  mitigation; it is review discipline, not an enforced check. A CI check that a
  PR touching client render paths also touches the coverage table is the
  cheapest escalation if that proves leaky.
- Per-room corpus depth must stay under `AXON_SYNC__TIMELINE_LIMIT=200` until
  true historical backfill (issue 164) lands.
- `smoke/` crates still may not depend on any `axon-*` crate
  (`scripts/check-smoke-isolation.sh`); `axon-demo-tui` inherits that rule.
- **The two sections above are decided but not built**, and are tracked in #111
  rather than left implicit. They were omitted from the first draft of this ADR
  and consequently went unbuilt and unnoticed through four PRs — recording them
  here, even unbuilt, is what stops that recurring. Until they land, the demo
  corpus illustrates the product without testing it, and the only thing checking
  a recording is a human watching one.

## Alternatives considered

**Record against the Axon Testing room.** Zero new infrastructure, and the
content is genuinely real. Rejected: it is a shared mutable room on someone
else's homeserver, so recordings are unreproducible, may capture real
account data, and cannot be re-shot identically after a UI change.

**Extend `mock-server.mjs` instead of the real stack.** Fastest path to a web
video and needs no Docker. Rejected: it yields no TUI story, adds nothing to
smoke testing, and demonstrates a mock rather than the product — search in
particular would be a substring match standing in for the Tantivy index.

**Headless TUI recording (VHS, or asciinema + `agg`).** Fully automated and
CI-able. Rejected as the primary path because neither renders Sixel or Kitty
graphics, so the TUI's most distinctive rendering would be reduced to halfblock
approximations. A human running a screen recorder is a small cost for showing
what the client actually does.

**Screenshot/visual-regression baselines gating PRs.** Would mechanically
prevent demo drift. Deferred: baseline maintenance is a well-known source of CI
flake, and the project has no visual-regression infrastructure to build on.
Revisit if the coverage-table convention proves insufficient.
