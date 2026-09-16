# Brand marks

The Axon mark on a solid background, for slides, documents, and anywhere else
outside the app itself.

| File                | Use it on                                                               |
| ------------------- | ----------------------------------------------------------------------- |
| `axon-mark-light.*` | white or near-white pages                                               |
| `axon-mark-dark.*`  | dark or coloured surfaces, and wherever the mark should read as a badge |

SVG and PNG of each; the PNGs are 1024×1024 and have **no alpha channel**, so
they paste into a document without a halo and without a transparency box.

For an in-app icon, use the artwork these come from — `clients/web/public/favicon.svg`
— which is transparent on purpose, so a tab, a launcher and a Dock can each
composite it onto their own chrome.

## Do not edit these

They are generated, and the generator will overwrite them:

```sh
scripts/build-brand-assets.py
```

Change `clients/web/public/favicon.svg` and rerun that, plus `tauri icon` for
the desktop set — the `icons-regenerated` pre-push hook requires everything
derived from the master to move with it.

## Colours

- Glyph: `#5142E6`
- Light backdrop: `#EEEDFD`, a pale tint of the glyph (5.6:1 against it)
- Dark backdrop: `#5142E6` with a white glyph (6.4:1)

Deliberately not the true complement of the brand colour. That is `#D7E642`, a
yellow-green, and it is both off-brand and the worst of the candidates on
contrast at 4.7:1 — complementary colours maximise hue separation, which is not
the same as being legible.
