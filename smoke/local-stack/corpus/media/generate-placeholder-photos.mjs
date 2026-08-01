// Writes stand-in files for the six `photos/` images the demo corpus expects,
// so the seeder and both demo drivers can be built and exercised end-to-end
// before any real photograph has been chosen (ADR 0086).
//
// These are NOT committed and must never reach a published recording. Each one
// is stamped PLACEHOLDER across the middle so that if one does survive into a
// take, it is obvious in the first frame rather than after upload.
//
// Usage — same constraint as generate-avatars.mjs: Node resolves ESM imports
// from the script's own directory, so copy it next to its node_modules.
//
//   mkdir -p /tmp/axon-photos && cd /tmp/axon-photos
//   npm install sharp@^0.34
//   cp "$REPO"/smoke/local-stack/corpus/media/generate-placeholder-photos.mjs .
//   node generate-placeholder-photos.mjs "$REPO"/smoke/local-stack/corpus/media/photos
//
// Dimensions and file names match what corpus/media/README.md specifies, so the
// gallery grid, lightbox paging, and thumbnail proxy all see realistic geometry
// (1600px long edge, 3:2 landscape) rather than square test cards.

import { mkdir, writeFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import sharp from 'sharp'

const WIDTH = 1600
const HEIGHT = 1067 // 3:2, the aspect most cameras produce

// Distinct hues so the four gallery-run images are individually recognisable in
// a grid and while paging the lightbox — a run of identical placeholders would
// hide an ordering or paging bug rather than reveal it.
const PHOTOS = [
  { name: 'switchback-14-october', label: 'Switchback 14 — October', hue: 28 },
  { name: 'party-crew', label: 'Party crew', hue: 205 },
  { name: 'switchback-14-before', label: 'Switchback 14 — before', hue: 12 },
  { name: 'switchback-14-crib', label: 'Switchback 14 — cribbing', hue: 96 },
  { name: 'switchback-14-after', label: 'Switchback 14 — after', hue: 150 },
  { name: 'talus-light', label: 'Talus, low sun', hue: 262 },
]

const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')

function svg({ label, hue }) {
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${WIDTH}" height="${HEIGHT}">
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0%" stop-color="hsl(${hue} 42% 62%)"/>
      <stop offset="100%" stop-color="hsl(${(hue + 40) % 360} 38% 34%)"/>
    </linearGradient>
  </defs>
  <rect width="${WIDTH}" height="${HEIGHT}" fill="url(#g)"/>
  <text x="50%" y="46%" text-anchor="middle" font-family="sans-serif"
        font-size="128" font-weight="700" fill="#ffffff" opacity="0.85"
        letter-spacing="14">PLACEHOLDER</text>
  <text x="50%" y="57%" text-anchor="middle" font-family="sans-serif"
        font-size="52" fill="#ffffff" opacity="0.9">${esc(label)}</text>
  <text x="50%" y="64%" text-anchor="middle" font-family="sans-serif"
        font-size="34" fill="#ffffff" opacity="0.7">not for publication</text>
</svg>`
}

const outDir = process.argv[2]
if (!outDir) {
  console.error('usage: node generate-placeholder-photos.mjs <path to corpus/media/photos>')
  process.exit(1)
}
await mkdir(outDir, { recursive: true })

let total = 0
for (const photo of PHOTOS) {
  const buf = await sharp(Buffer.from(svg(photo))).jpeg({ quality: 78 }).toBuffer()
  await writeFile(resolve(outDir, `${photo.name}.jpg`), buf)
  total += buf.length
  console.log(`${photo.name.padEnd(24)} ${String(buf.length).padStart(7)} bytes`)
}
console.log(`${PHOTOS.length} placeholders, ${(total / 1024).toFixed(1)} KiB total`)
console.log('These are stand-ins. Replace before recording; see README.md.')
