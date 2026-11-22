import assert from 'node:assert/strict'
import { test } from 'node:test'
import { coordinatorDisplayState, providerDisplayState } from '../src/provider-state.ts'

test('unsupported unconfirmed hardware is visibly blocked before starting', () => {
  assert.deepEqual(
    providerDisplayState({
      running: false,
      connection: 'disconnected',
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

test('coordinator connection state is explicit in the status labels', () => {
  assert.deepEqual(coordinatorDisplayState('connecting'), {
    label: 'Connecting to coordinator',
    status: 'Connecting',
  })
  assert.deepEqual(coordinatorDisplayState('connected'), {
    label: 'Coordinator connected',
    status: 'Connected',
  })
  assert.deepEqual(coordinatorDisplayState('disconnected'), {
    label: 'Coordinator unreachable',
    status: 'Coordinator unreachable',
  })
})

test('a running provider shows connection recovery instead of an online state', () => {
  assert.deepEqual(
    providerDisplayState({
      running: true,
      connection: 'disconnected',
      confirmationRequired: false,
      jobId: null,
      policyReason: '',
      userPaused: false,
    }),
    {
      label: 'Coordinator unreachable',
      detail: 'Waiting to reconnect to the coordinator.',
    },
  )
})
