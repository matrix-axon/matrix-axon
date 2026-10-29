import { fireEvent, render, screen } from '@testing-library/preact'
import { expect, it, vi } from 'vitest'
import { Composer } from './Composer'
import type { EmojiPickerProps } from './EmojiPicker'

vi.mock('./EmojiPicker', () => ({
  EmojiPicker: ({ onSelect, onClose }: EmojiPickerProps) => (
    <div role="dialog" aria-label="Emoji picker">
      <button type="button" onClick={() => onSelect('🙂')}>
        Unicode
      </button>
      <button
        type="button"
        onClick={() =>
          onSelect('mxc://first/smile', 'smile', {
            url: 'mxc://first/smile',
            shortcode: 'smile',
            body: null,
            info: null,
            emoticon: true,
            sticker: true,
          })
        }
      >
        First
      </button>
      <button
        type="button"
        onClick={() =>
          onSelect('mxc://second/smile', 'smile', {
            url: 'mxc://second/smile',
            shortcode: 'smile',
            body: null,
            info: null,
            emoticon: true,
            sticker: true,
          })
        }
      >
        Second
      </button>
      <button type="button" onClick={onClose}>
        Close
      </button>
    </div>
  ),
}))

function setup(initialValue = 'hello world') {
  const onSubmit = vi.fn(async () => true)
  const onDraftChange = vi.fn()
  const view = render(
    <Composer
      placeholder="Message"
      initialValue={initialValue}
      onSubmit={onSubmit}
      onDraftChange={onDraftChange}
    />,
  )
  const input = screen.getByRole('textbox') as HTMLTextAreaElement
  const open = (start: number, end = start) => {
    input.focus()
    input.setSelectionRange(start, end)
    fireEvent.keyDown(input, { key: ';', ctrlKey: true })
  }
  return { ...view, input, open, onSubmit, onDraftChange }
}

it('opens only for the primary-modifier chord and inserts at the saved selection', () => {
  const { input, open, onDraftChange } = setup()
  fireEvent.keyDown(input, { key: ';', ctrlKey: true, altKey: true })
  expect(screen.queryByRole('dialog')).toBeNull()
  fireEvent.keyDown(input, { key: ';', ctrlKey: true, isComposing: true })
  expect(screen.queryByRole('dialog')).toBeNull()
  open(6, 11)
  fireEvent.click(screen.getByRole('button', { name: 'Unicode' }))
  expect(input.value).toBe('hello 🙂')
  expect(input.selectionStart).toBe(8)
  expect(onDraftChange).toHaveBeenLastCalledWith('hello 🙂')
  expect(screen.queryByRole('dialog')).toBeNull()
})

it('keeps the exact image on insertion, replacement, formatting, and submission', () => {
  const { input, open, onSubmit, onDraftChange } = setup('hi ')
  open(3)
  fireEvent.click(screen.getByRole('button', { name: 'First' }))
  expect(input.value).toBe('hi :smile:')
  expect(onDraftChange).toHaveBeenLastCalledWith('hi :smile:', [
    { start: 3, end: 10, shortcode: 'smile', mxc: 'mxc://first/smile' },
  ])
  open(3, 10)
  fireEvent.click(screen.getByRole('button', { name: 'Second' }))
  input.setSelectionRange(3, 10)
  fireEvent.keyDown(input, { key: 'b', ctrlKey: true })
  expect(input.value).toBe('hi **:smile:**')
  fireEvent.submit(input.closest('form')!)
  expect(onSubmit).toHaveBeenCalledWith('hi **:smile:**', [
    { start: 5, end: 12, shortcode: 'smile', mxc: 'mxc://second/smile' },
  ])
  expect(input.value).toBe('')
})

it('restores bindings with native history input events and trims offsets on send', () => {
  const { input, open, onSubmit } = setup('  hi ')
  open(5)
  fireEvent.click(screen.getByRole('button', { name: 'First' }))
  fireEvent.input(input, {
    target: { value: '  hi ' },
    inputType: 'historyUndo',
  })
  fireEvent.input(input, {
    target: { value: '  hi :smile:' },
    inputType: 'historyRedo',
  })
  fireEvent.submit(input.closest('form')!)
  expect(onSubmit).toHaveBeenCalledWith('hi :smile:', [
    { start: 3, end: 10, shortcode: 'smile', mxc: 'mxc://first/smile' },
  ])
})

it('drops the binding when the shortcode itself is edited', () => {
  const { input, open, onSubmit } = setup('hi ')
  open(3)
  fireEvent.click(screen.getByRole('button', { name: 'First' }))
  fireEvent.input(input, { target: { value: 'hi :smiles:' } })
  fireEvent.submit(input.closest('form')!)
  expect(onSubmit).toHaveBeenCalledWith('hi :smiles:')
})
