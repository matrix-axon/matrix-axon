import { describe, expect, it } from 'vitest'
import { maxByArrivalOrder } from './arrival-order'

const at = (id: string, arrival_order: number) => ({
  event_id: id,
  arrival_order,
})

describe('maxByArrivalOrder', () => {
  it('returns null for an empty input', () => {
    expect(maxByArrivalOrder([])).toBeNull()
  })

  it('picks the greatest arrival_order regardless of position', () => {
    expect(
      maxByArrivalOrder([at('$a', 5), at('$c', 9), at('$b', 7)])?.event_id,
    ).toBe('$c')
  })

  it('keeps the last of equal arrival orders', () => {
    // Not incidental: bridges stamp bursts within a single millisecond, so ties
    // are common rather than exotic, and the later row is the one a page renders
    // last. The TUI's `read_targets_for` chose the same rule, and the three call
    // sites this helper replaced did not all agree — two used `>` and one `>=`.
    expect(
      maxByArrivalOrder([at('$first', 4), at('$second', 4)])?.event_id,
    ).toBe('$second')
  })

  it('ignores earlier ties below the maximum', () => {
    expect(
      maxByArrivalOrder([at('$a', 4), at('$b', 4), at('$top', 6)])?.event_id,
    ).toBe('$top')
  })
})
