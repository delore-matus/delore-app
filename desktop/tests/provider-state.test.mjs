import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
  coordinatorDisplayState,
  preparationDisplayState,
  providerDisplayState,
} from '../src/provider-state.ts'

test('unsupported unconfirmed hardware is visibly blocked before starting', () => {
  assert.deepEqual(
    providerDisplayState({
      running: false,
      connection: 'disconnected',
      preparing: false,
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

test('startup is visibly preparing without a false coordinator error', () => {
  assert.deepEqual(
    providerDisplayState({
      running: true,
      connection: 'disconnected',
      preparing: true,
      confirmationRequired: false,
      jobId: null,
      policyReason: '',
      userPaused: false,
    }),
    {
      label: 'Preparing',
      detail: 'Preparing DeLore before connecting to the coordinator.',
    },
  )
  assert.deepEqual(preparationDisplayState('starting', 0), {
    label: 'Preparing DeLore',
    detail: 'Starting DeLore. This can take up to 3 minutes.',
    indeterminate: true,
  })
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
      preparing: false,
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
