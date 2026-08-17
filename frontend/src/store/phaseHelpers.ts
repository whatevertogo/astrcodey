import type { ConversationControlState, Phase } from '../services/types'

/** 后端控制状态叠加本地 compact 请求窗口后的唯一 UI phase。 */
export function effectiveConversationPhase(
  control: ConversationControlState | null,
  compactSubmitting: boolean
): Phase {
  return compactSubmitting ? 'compacting' : (control?.phase ?? 'idle')
}

export function isExecutionPhase(phase: Phase): boolean {
  return (
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
  if (effectiveConversationPhase(control, compactSubmitting) === 'compacting') {
    return false
  }
  return !!control?.activeTurnId
}
