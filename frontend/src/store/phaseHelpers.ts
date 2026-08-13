import type { ConversationControlState, Phase } from '../services/types'

export function isExecutionPhase(
  phase: Phase,
  compactSubmitting: boolean
): boolean {
  return (
    compactSubmitting ||
    phase === 'thinking' ||
    phase === 'streaming' ||
    phase === 'calling_tool' ||
    phase === 'compacting'
  )
}

/** 与后端 TurnRegistry 对齐：仅在有 active turn 时可 inject。 */
export function canInjectMidTurn(
  control: ConversationControlState | null,
  compactSubmitting: boolean
): boolean {
  if (compactSubmitting || control?.phase === 'compacting') {
    return false
  }
  return !!control?.activeTurnId
}
