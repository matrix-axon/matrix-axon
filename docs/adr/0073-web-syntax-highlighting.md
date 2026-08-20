# ADR 0073 — Web client syntax highlighting

## Context

ADR 0072 gave text attachments an inline preview: a `.py` or `.sh` file renders
as a scrollable `<pre>`. Unstyled, which is exactly as useful as `cat` and
noticeably less useful than the editor the file came from.

The gap is wider than attachments. The sanitizer already goes to the trouble of
preserving spec-shaped `language-*` classes on `<code>`
(`clients/web/src/html/sanitize.ts`) — Matrix's only signal for a fenced block's
language — and **nothing consumes them**. A `python` block pasted into a message
has carried its language all along with nothing to read it.

So both surfaces are unhighlighted, for the same missing reason, and shipping a
highlighter for one and not the other would mean the same file renders two
different ways depending on whether it was pasted or attached.

## Decision

### `highlight.js`, core + per-language, never the default bundle

`highlight.js`'s default entry point registers ~190 grammars and is roughly a
megabyte. Importing `highlight.js/lib/core` and registering grammars
individually is a different proposition: the engine is ~21 KB (8 KB gzipped)
and a grammar is 2–8 KB.

Rejected alternatives:

- **Shiki** produces better output — real TextMate grammars, VS Code themes —
  but carries a WASM regex engine and large grammar payloads. Wrong trade for
  coloring a shell script in a chat client.
- **Prism** is comparable in output but tree-shakes worse.

### One chunk per grammar, loaded on use

`clients/web/src/code/languages.ts` holds a `Record<string, () => import(...)>`
of ~30 languages. Vite emits each as its own chunk, so opening a Python file
fetches the core plus `python` and nothing else. Adding a language to the table
costs the main bundle nothing.

Measured on this change: the main bundle grows 353 → 360 KB (107 → 109 KB
gzipped) for the tables and wiring; the engine and grammars are separate chunks
that never load until a preview or a code block asks for one.

### The language comes from the name, not from guessing

`highlight.js` can auto-detect, but doing so means loading many grammars to
compare against — the opposite of the above. Every path here names the language
instead:

- a message code block declares it (`class="language-python"`);
- an attachment's extension implies it, via an alias table (`py`, `sh`, `yml`);
- a `Makefile`/`Dockerfile`/`.bashrc` is recognised whole;
- an **extensionless script** falls back to its shebang. `#!/usr/bin/env
  python3` → `python`, `#!/bin/bash -eu` → `bash`, version suffixes stripped.
  Scripts are routinely uploaded as `deploy` or `backup` with no extension, and
  the first line is then the only thing that identifies them.

An unrecognised language renders as plain text — what it did before.

### Highlighting is a decoration applied after the text is readable

The preview paints plain text as soon as the bytes arrive, then upgrades it when
the highlighter chunk resolves. Awaiting a lazy chunk before the first paint
would put a readable file behind a network round trip.

A **128 KB source cap** applies: `highlight.js` tokenises synchronously on the
main thread, so a large log would be a visible stall — and nobody reads a 200 KB
log in color anyway. Above the cap, plain text.

### Output is sanitized even though it does not have to be

`highlight.js` escapes its input and emits only `<span class>`. The output is
still passed through DOMPurify (already a dependency) with
`ALLOWED_TAGS: ['span']`. This is the one place in the app where a string
becomes `innerHTML` without going through `renderMatrixHtml`, and the cost is
microseconds. The test asserts the property that matters — that parsing the
output yields no element but `span`, and that its text is byte-identical to the
source — rather than asserting the absence of a substring, since a payload
legitimately appears *as text*.

### Message blocks are highlighted imperatively

`FormattedBody` already post-processes its `dangerouslySetInnerHTML` subtree in
a `useLayoutEffect` (resolving `img[data-mxc]` to blob URLs). Code blocks get a
second effect in the same shape, guarded by re-checking `textContent` before
writing, so a resolution that lands after the content changed is dropped.

## Consequences

- The palette is ours (CSS custom properties in the app's three theme blocks),
  not a stylesheet from the package — importing one would put a fixed theme in
  the main CSS bundle and fight the app's own light/dark tokens.
- ~30 languages are supported. The list is a table; extending it is one line and
  no bundle cost.
- Highlighting appears a frame or two after the text on first use, while the
  chunk loads. Subsequent files reuse the cached engine.

## Follow-ups not taken

- **Auto-detection** for files that name no language. Needs many grammars
  resident to be meaningful; the shebang fallback covers the realistic case.
- **A line-numbered / foldable code viewer.** That is an editor, not a preview.
