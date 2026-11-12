import { expect, test } from '@playwright/test'
import { expectRoomReady, ROOM_URL, signIn } from './helpers'

/**
 * On a phone, the message header is one line: name, time, `(edited)`, and the
 * action bar. Before this, `.event-head` wrapped, so an edited message or a
 * very long display name pushed the open action bar onto a second line and left
 * a blank gap between the header and the body.
 *
 * The mock has no edited message and no long names, so the spec manufactures
 * both on a real rendered row (an `(edited)` button and a 200-character name)
 * and opens the bar with the same `actions-open` class the app sets. That
 * exercises the stylesheet, which is the thing under test.
 */
for (const width of [320, 375]) {
  test(`the open action bar stays on the name line at ${width}px for an edited message and a long name`, async ({
    page,
  }) => {
    await signIn(page)
    await page.setViewportSize({ width, height: 800 })
    await page.goto(ROOM_URL)
    await expectRoomReady(page)

    const result = await page.evaluate(() => {
      const row = [
        ...document.querySelectorAll<HTMLElement>('.event-row'),
      ].find((el) => el.querySelector('.event-head > .event-actions'))
      if (!row) return null
      const head = row.querySelector<HTMLElement>('.event-head')!
      const sender = head.querySelector<HTMLElement>('.event-sender')!
      const actions = head.querySelector<HTMLElement>('.event-actions')!
      const edited = document.createElement('button')
      edited.className = 'ghost edited-marker'
      edited.textContent = '(edited)'
      head.insertBefore(edited, actions)
      sender.textContent = 'A'.repeat(200)
      const measure = () => ({
        senderTop: sender.getBoundingClientRect().top,
        actionsTop: actions.getBoundingClientRect().top,
        actionsRight: actions.getBoundingClientRect().right,
        headRight: head.getBoundingClientRect().right,
        editedDisplay: getComputedStyle(edited).display,
      })
      row.classList.add('actions-open')
      return measure()
    })
    expect(result).not.toBeNull()
    // Same line: the bar's top is within a few pixels of the name's top.
    expect(Math.abs(result!.actionsTop - result!.senderTop)).toBeLessThan(16)
    // And it fits inside the header rather than overflowing it.
    expect(result!.actionsRight).toBeLessThanOrEqual(result!.headRight + 1)
    // The edit marker yields while the bar is open.
    expect(result!.editedDisplay).toBe('none')
  })
}
