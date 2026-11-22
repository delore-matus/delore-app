import assert from 'node:assert/strict'
import { test } from 'node:test'
import { isValidPerformancePercent } from '../src/performance.ts'

test('performance slider accepts its full 0-100 range and rejects invalid values', () => {
  assert.equal(isValidPerformancePercent(0), true)
  assert.equal(isValidPerformancePercent(1), true)
  assert.equal(isValidPerformancePercent(100), true)
  assert.equal(isValidPerformancePercent(-1), false)
  assert.equal(isValidPerformancePercent(101), false)
  assert.equal(isValidPerformancePercent(50.5), false)
})
