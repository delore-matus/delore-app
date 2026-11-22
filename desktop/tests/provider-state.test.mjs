import assert from 'node:assert/strict'
import { test } from 'node:test'
import { providerDisplayState } from '../src/provider-state.ts'

test('unsupported unconfirmed hardware is visibly blocked before starting', () => {
  assert.deepEqual(
    providerDisplayState({
      running: false,
      confirmationRequired: true,
      jobId: null,
      policyReason: '',
      userPaused: false,
    }),
    {
      label: 'Needs confirmation',
      detail: 'Confirm hardware use before starting.',
    },
  )
})
