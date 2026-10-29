import { render } from '@testing-library/preact'
import { expect, it } from 'vitest'
import { LinkPreviews } from './LinkPreviews'

it('keeps additional previews expanded when their URLs reorder', () => {
  const urls = [
    'https://one.test/',
    'https://two.test/',
    'https://three.test/',
    'https://four.test/',
  ]
  const body = urls.join(' ')
  const content = (order: string[]) => ({
    'm.url_previews': order.map((url) => ({
      'matrix:matched_url': url,
      'og:title': url,
    })),
  })
  const view = render(
    <LinkPreviews accountId="first" body={body} content={content(urls)} />,
  )
  const disclosure = view.container.querySelector('details')!
  disclosure.open = true
  view.rerender(
    <LinkPreviews
      accountId="first"
      body={body}
      content={content([...urls].reverse())}
    />,
  )
  expect(view.container.querySelector('details')!.open).toBe(true)
  expect(view.container.querySelector('details')!.textContent).toContain(
    urls[0],
  )
})
