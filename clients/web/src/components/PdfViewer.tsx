import { useEffect, useRef, useState } from 'preact/hooks'
import { observeVisible } from '../media/intersection'

/**
 * A paged PDF viewer (ADR 0073's follow-on, recorded in ADR 0072).
 *
 * An `<iframe>` was the obvious way to show a PDF and it is broken where it
 * matters most: iOS Safari renders only the *first page* of a framed PDF, with
 * no scrolling and no page controls, so a multi-page document was unreadable on
 * a phone. That is a WebKit restriction on framed PDFs — no amount of sizing
 * fixes it — so the document has to be drawn rather than embedded.
 *
 * `pdfjs-dist` is loaded through a dynamic `import()`, keeping ~440 KB of
 * viewer and a worker out of the main bundle; nothing is fetched until someone
 * opens a PDF. Pages render to `<canvas>` lazily as they scroll into view,
 * reusing the same `observeVisible` helper the media layer uses, so a 200-page
 * document does not rasterise 200 canvases up front.
 */
export function PdfViewer({ url, label }: { url: string; label: string }) {
  const containerRef = useRef<HTMLDivElement>(null)
  const [pageCount, setPageCount] = useState<number | null>(null)
  const [currentPage, setCurrentPage] = useState(1)
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    const container = containerRef.current
    if (container === null) {
      return
    }
    let cancelled = false
    const cleanups: (() => void)[] = []
    // `destroy()` lives on the loading task, not the document proxy.
    let task: { destroy: () => Promise<void> } | null = null

    void (async () => {
      try {
        const pdfjs = await import('pdfjs-dist')
        // The worker is a separate asset; Vite rewrites this URL at build time.
        pdfjs.GlobalWorkerOptions.workerSrc = (
          await import('pdfjs-dist/build/pdf.worker.min.mjs?url')
        ).default

        const loading = pdfjs.getDocument({ url })
        task = loading
        const loaded = await loading.promise
        if (cancelled) {
          void loading.destroy()
          return
        }
        setPageCount(loaded.numPages)

        for (let number = 1; number <= loaded.numPages; number += 1) {
          const holder = document.createElement('div')
          holder.className = 'pdf-page'
          holder.dataset.page = String(number)
          container.appendChild(holder)

          let rendered = false
          const render = () => {
            if (rendered || cancelled) {
              return
            }
            rendered = true
            void loaded.getPage(number).then((page) => {
              if (cancelled) {
                return
              }
              // Fit the page to the container, then scale for the device's
              // pixel ratio so text is not soft on a phone.
              const unscaled = page.getViewport({ scale: 1 })
              const fit = holder.clientWidth / unscaled.width
              const ratio = Math.min(window.devicePixelRatio || 1, 2)
              const viewport = page.getViewport({ scale: fit * ratio })
              const canvas = document.createElement('canvas')
              canvas.width = viewport.width
              canvas.height = viewport.height
              canvas.style.width = '100%'
              const context = canvas.getContext('2d')
              if (context === null) {
                return
              }
              holder.replaceChildren(canvas)
              // `render()` returns a task, and the failure arrives on its
              // `promise` — discarding the task discards the only channel a
              // failed page has. That is why a blank page went unnoticed until
              // someone opened a PDF on Linux: pdf.js reported success, drew
              // nothing, and there was nowhere for a real error to surface
              // either. Reported rather than rendered into the page, because a
              // page that fails to draw is a bug to fix, not a state to design.
              void page
                .render({ canvas, canvasContext: context, viewport })
                .promise.catch((error: unknown) => {
                  console.error('could not render PDF page', number, error)
                })
            })
          }

          // Reserve each page's height before it renders, so the scrollbar is
          // honest from the start and lazy rendering cannot make the document
          // jump under the reader's finger.
          void loaded.getPage(number).then((page) => {
            if (cancelled) {
              return
            }
            const viewport = page.getViewport({ scale: 1 })
            holder.style.aspectRatio = `${viewport.width} / ${viewport.height}`
          })

          cleanups.push(observeVisible(holder, render))
        }
      } catch {
        if (!cancelled) {
          setFailed(true)
        }
      }
    })()

    return () => {
      cancelled = true
      for (const stop of cleanups) {
        stop()
      }
      void task?.destroy()
      container.replaceChildren()
    }
  }, [url])

  // Which page is in view, for the counter. Cheap: a scroll handler reading
  // offsets, not another observer per page.
  const onScroll = (event: Event) => {
    const container = event.currentTarget as HTMLElement
    const middle = container.scrollTop + container.clientHeight / 2
    let page = 1
    for (const holder of container.querySelectorAll<HTMLElement>('.pdf-page')) {
      if (holder.offsetTop <= middle) {
        page = Number(holder.dataset.page ?? '1')
      }
    }
    setCurrentPage(page)
  }

  if (failed) {
    return (
      <p class="muted placeholder">
        Could not render this PDF — use Download to open it in another app.
      </p>
    )
  }

  return (
    <div class="pdf-viewer">
      <div
        ref={containerRef}
        class="pdf-pages"
        tabindex={0}
        role="document"
        aria-label={label}
        onScroll={onScroll}
      />
      {pageCount === null ? (
        <div class="media-skeleton" aria-hidden="true" />
      ) : (
        <span class="pdf-page-count" aria-live="polite">
          {currentPage} / {pageCount}
        </span>
      )}
    </div>
  )
}
