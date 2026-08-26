import { cleanup, render } from '@testing-library/preact'
import { afterEach, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import { testServices } from '../test/services'
import { MediaCaption } from './MediaCaption'

const ACCOUNT = '11111111-1111-4111-8111-111111111111'

afterEach(() => cleanup())

function renderCaption(caption: string, content?: unknown) {
  return render(
    <ServicesContext.Provider value={testServices()}>
      <MediaCaption accountId={ACCOUNT} caption={caption} content={content} />
    </ServicesContext.Provider>,
  )
}

describe('MediaCaption', () => {
  it('renders markdown the same way a message body does', () => {
    const { container } = renderCaption('a **bold** caption')
    expect(container.querySelector('strong')?.textContent).toBe('bold')
    expect(container.textContent).toBe('a bold caption')
  })

  it('leaves a plain caption as text', () => {
    const { container } = renderCaption('look at this')
    expect(container.querySelector('strong')).toBeNull()
    expect(container.textContent).toBe('look at this')
  })

  it('prefers the event formatted_body over caption markdown', () => {
    const { container } = renderCaption('**ignored**', {
      format: 'org.matrix.custom.html',
      formatted_body: '<p><em>from html</em></p>',
    })
    expect(container.querySelector('em')?.textContent).toBe('from html')
    expect(container.querySelector('strong')).toBeNull()
  })
})
