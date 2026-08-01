# Demo corpus media assets

`demo.toml` references image files under this directory. **They are not
committed yet** — sourcing them is an open decision (ADR 0086 records the
tradeoff). The seeder should fail loudly with the missing path rather than
silently seeding a room with no images.

## Required files

Referenced by `demo.toml`; every path below must exist before
`axon-smoke-local-stack up --corpus …/demo.toml` can complete.

### `avatars/` — 8 files

Square, 256×256, PNG. Six personas plus two space avatars:

| File | For |
|---|---|
| `alex.png` | Alex Chen (the viewer account) |
| `maya.png` | Maya Okonkwo |
| `devin.png` | Devin Ruiz |
| `sam.png` | Sam Lindqvist |
| `priya.png` | Priya Raman |
| `tomas.png` | Tomás Herrera |
| `space-ridgeline.png` | Ridgeline Trail Collective space |
| `space-watershed.png` | Cascadia Watershed Council space |

Avatars must **not** be photographs of real people. Generated abstract or
monogram avatars are the right answer here regardless of how the photos below
are resolved — a fictional persona wearing a real person's face is the one
outcome to avoid.

### `photos/` — 6 files

Landscape, 1600px on the long edge, JPEG. These carry the gallery and lightbox
demos, so they are the files whose realism actually matters.

| File | Subject | Role in the demo |
|---|---|---|
| `switchback-14-october.jpg` | Trail switchback, autumn | Standalone image, three weeks back |
| `party-crew.jpg` | Group of volunteers with tools | Gallery run, image 1 of 4 |
| `switchback-14-before.jpg` | Eroded trail corner | Gallery run, image 2 of 4 |
| `switchback-14-crib.jpg` | Rock cribbing under construction | Gallery run, image 3 of 4 |
| `switchback-14-after.jpg` | Rebuilt trail tread | Gallery run, image 4 of 4 |
| `talus-light.jpg` | Rocky slope, low sun | Adjacency near-miss case |

The four `switchback-14-*` / `party-crew` files are posted seconds apart by one
sender and must read as one coherent set — ADR 0081's adjacency heuristic groups
them into a single grid, and the web demo pages through them in the lightbox.
`talus-light.jpg` is deliberately posted four minutes later with a text message
between, so it must **not** join that grid; it is the standing regression case
for the grouping heuristic.

## Sourcing: open decision

Two viable options, neither yet chosen:

**CC0 photographs.** Look genuinely real in a gallery, which is the whole point
of the exercise. Cost: fixed weight in a repo whose history is currently 3.8 MB
with no LFS, and each file needs its source and license recorded below.

**Procedurally generated images.** Deterministic, tiny, license-clean, and
regenerable by a committed script rather than stored. Cost: they look synthetic,
which undercuts a gallery demo whose purpose is to look like a real one.

A reasonable split is generated avatars (where realism is undesirable anyway)
and CC0 photographs for `photos/` (where it is the point). Budget suggestion if
photographs are chosen: **≤120 KB per file after downscaling**, so all six add
under 1 MB to the repo.

## Provenance

Fill this in as files land. Every photographic file needs a row.

<!-- prettier-ignore -->
| File | Source | License | Notes |
|---|---|---|---|
| _(none yet)_ | | | |
