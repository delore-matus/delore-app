export interface ProviderDisplayInput {
  running: boolean
  connection: 'connecting' | 'connected' | 'disconnected'
  confirmationRequired: boolean
  jobId: string | null
  policyReason: string
  userPaused: boolean
}

export interface ProviderDisplayState {
  label: string
  detail: string
}

export interface CoordinatorDisplayState {
  label: string
  status: string
}

export function coordinatorDisplayState(
  connection: 'connecting' | 'connected' | 'disconnected',
): CoordinatorDisplayState {
  switch (connection) {
    case 'connected':
      return { label: 'Coordinator connected', status: 'Connected' }
    case 'connecting':
      return { label: 'Connecting to coordinator', status: 'Connecting' }
    case 'disconnected':
      return { label: 'Coordinator unreachable', status: 'Coordinator unreachable' }
  }
}

export function providerDisplayState(input: ProviderDisplayInput): ProviderDisplayState {
  if (!input.running && input.confirmationRequired) {
    return {
      label: 'Needs confirmation',
      detail: 'Confirm hardware use before starting.',
    }
  }
  if (!input.running) {
    return { label: 'Offline', detail: 'Start DeLore to accept work.' }
  }
  if (input.connection === 'connecting') {
    return { label: 'Connecting', detail: 'Waiting for the coordinator.' }
  }
  if (input.connection === 'disconnected') {
    return {
      label: 'Coordinator unreachable',
      detail: 'Waiting to reconnect to the coordinator.',
    }
  }
  if (input.jobId !== null) {
    return { label: 'Running a job', detail: `Job ${input.jobId} is in progress.` }
  }
  if (input.policyReason) {
    return { label: 'Paused', detail: input.policyReason }
  }
  if (input.userPaused) {
    return {
      label: 'Pausing',
      detail: 'The current job will finish before new work pauses.',
    }
  }
  return { label: 'Online', detail: 'Ready to accept work.' }
}
