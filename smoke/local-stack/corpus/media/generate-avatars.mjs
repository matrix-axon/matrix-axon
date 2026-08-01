// Regenerates the demo corpus avatars in `avatars/` (ADR 0086).
//
// The outputs are committed, so you only need this to change a persona, add
// one, or re-tune the palette. Everything is deterministic: the seed is the
// output filename stem, so a given persona's face never changes across runs.
//
// Usage — Node resolves ESM imports from the *script's* directory, so the
// script has to sit next to the node_modules it needs. Copy it into a scratch
// directory rather than running it in place:
//
//   mkdir -p /tmp/axon-avatars && cd /tmp/axon-avatars
//   npm install @dicebear/core@^9 @dicebear/collection@^9 sharp@^0.34
//   cp "$REPO"/smoke/local-stack/corpus/media/generate-avatars.mjs .
//   node generate-avatars.mjs "$REPO"/smoke/local-stack/corpus/media/avatars
//
// Pin @dicebear/core and @dicebear/collection to the same major — v9 core with
// v10 collection is a peer-dependency conflict npm will refuse to install.
//
// The dependencies are deliberately NOT vendored into the repo: this is a
// one-shot asset generator, not part of any build, and adding a package.json
// under smoke/ would pull a Node toolchain into a Rust silo and into
// THIRDPARTY.md for no runtime benefit.
//
// Licensing (verified from each style's own `meta`, not from documentation):
//   - Open Peeps — Pablo Stanley, CC0 1.0 — https://www.openpeeps.com/
//   - Shapes     — DiceBear,      CC0 1.0
//   - Rings      — DiceBear,      CC0 1.0
//   - DiceBear itself is MIT.
// Check `style.meta.license` before adopting another style: the collection is
// not uniformly CC0 — `icons` is Bootstrap Icons under MIT, which would drag an
// attribution notice into a directory that currently needs none.
// CC0 requires no attribution, but corpus/media/README.md records provenance
// anyway so a later reader never has to re-derive it.

import { mkdir, writeFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { createAvatar } from '@dicebear/core'
import { openPeeps, rings, shapes } from '@dicebear/collection'
import sharp from 'sharp'

const SIZE = 256

// Muted, desaturated backgrounds. Avatars sit against both the light and dark
// themes (clients/web/src/index.css) and next to the 8-colour room-avatar
// palette, so anything saturated fights the UI rather than sitting in it.
const BACKGROUNDS = ['c8d8e4', 'd6cdea', 'cfe0d3', 'ecd9c6', 'd9d9e3', 'e4d4d4']

// Open Peeps' full face set spans `rage`, `veryAngry`, `monster` and `cyclops`.
// Left unconstrained, seeds land on alarmed or hostile expressions and the
// member list reads like a hostage video rather than a volunteer group. Narrow
// it to friendly-to-neutral faces; `serious` and `driven` keep it from being
// uniformly grinning, and `smileTeethGap` is left out because it reads as manic
// at avatar size.
const FACES = [
  'calm',
  'cheeky',
  'smile',
  'smileBig',
  'lovingGrin1',
  'explaining',
  'driven',
  'serious',
]

// People. Seeds are the filename stems, which match each persona's `localpart`
// in demo.toml — keep the two in sync when adding a persona.
const PEOPLE = ['alex', 'maya', 'devin', 'sam', 'priya', 'tomas']

// Spaces. Open Peeps is people; a group needs an abstract mark instead.
const SPACES = ['space-ridgeline', 'space-watershed']

// Rooms. A different abstract style from the spaces, because the two appear
// side by side in axon-web — spaces in the left rail, rooms in the list — and
// one style for both would make a space read as just another room.
//
// `rings` rather than `glass`: at the ~24px a room list actually renders, glass
// resolves to six near-identical pale circles, which is worse than the letter
// tile it replaces. Rings stay distinguishable from each other when small.
// Seeds are the filename stems; the `room-` prefix matches each room's `id` in
// demo.toml. The DM is deliberately absent — a direct chat shows the other
// person's avatar, and giving it a room avatar would hide exactly the
// derivation the fixture exists to exercise.
const ROOMS = [
  'room-general',
  'room-trail-work',
  'room-trip-photos',
  'room-gear-swap',
  'room-water-quality',
  'room-field-notes',
]

async function render(style, seed, outDir, extra = {}) {
  const svg = createAvatar(style, {
    seed,
    size: SIZE,
    backgroundColor: BACKGROUNDS,
    radius: 50,
    ...extra,
  }).toString()

  // ratatui-image decodes through the `image` crate, which has no SVG support,
  // so an SVG avatar would render in axon-web and be invisible in axon-tui.
  // Rasterise here rather than shipping SVG to the homeserver.
  const png = await sharp(Buffer.from(svg)).png({ compressionLevel: 9 }).toBuffer()
  const path = resolve(outDir, `${seed}.png`)
  await writeFile(path, png)
  return { seed, bytes: png.length }
}

const outDir = process.argv[2]
if (!outDir) {
  console.error('usage: node generate-avatars.mjs <path to corpus/media/avatars>')
  process.exit(1)
}
await mkdir(outDir, { recursive: true })

const written = [
  ...(await Promise.all(PEOPLE.map((s) => render(openPeeps, s, outDir, { face: FACES })))),
  ...(await Promise.all(SPACES.map((s) => render(shapes, s, outDir)))),
  ...(await Promise.all(ROOMS.map((s) => render(rings, s, outDir)))),
]

let total = 0
for (const { seed, bytes } of written) {
  total += bytes
  console.log(`${seed.padEnd(18)} ${String(bytes).padStart(7)} bytes`)
}
console.log(`${written.length} files, ${(total / 1024).toFixed(1)} KiB total`)
