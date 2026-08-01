import type {
  AgentSessionLink,
  ConversationBlock,
  ConversationControlState,
  ConversationDelta,
  PendingAskUserQuestion,
} from '../../services/types'
import { decodePendingAskUserQuestion } from '../../services/protocol'
import type { AppState } from '../types'
import { mergeAgentSession, resolvePhase, upsertBlock } from './blockHelpers'
import {
  applyCoalescedDeltas,
  coalesceDeltas,
  type CoalescedDelta,
} from './coalesce'
import { applyConversationDeltaEffects } from './effects'

type BlockDelta = Exclude<CoalescedDelta, { kind: 'other' }>

export type ConversationRenderState = Pick<
  AppState,
  | 'blocks'
  | 'control'
  | 'cursor'
  | 'phase'
  | 'compactSubmitting'
  | 'agentSessions'
  | 'statusItems'
  | 'statusItemRevisions'
  | 'pendingAskUserQuestions'
  | 'resolvedAskUserCallIds'
  | 'pendingAskUserRefreshInFlight'
  | 'askUserEventRevision'
  | 'transientHint'
>

type ConversationRenderPatch = Partial<ConversationRenderState>

/** ToolCallId 仅在 turn 内唯一，跨会话可能碰撞，pending map 以 sessionId+callId 复合键索引。 */
export function pendingAskUserKey(sessionId: string, callId: string): string {
  return `${sessionId}:${callId}`
}

export function mergePendingAskUserSnapshot(
  currentPending: Record<string, PendingAskUserQuestion>,
  resolvedCallIds: Record<string, string>,
  snapshot: PendingAskUserQuestion[],
  pendingAtStart: ReadonlySet<string>,
  eventsArrivedDuringRequest: boolean
): Pick<
  ConversationRenderState,
  'pendingAskUserQuestions' | 'resolvedAskUserCallIds'
> {
  const pending: Record<string, PendingAskUserQuestion> = {}
  // 全局快照是权威基线；请求期间已 resolved 的条目不能被旧快照复活。
  for (const question of snapshot) {
    const key = pendingAskUserKey(question.sessionId, question.callId)
    if (eventsArrivedDuringRequest && resolvedCallIds[key]) continue
    pending[key] = question
  }
  // 请求期间经 SSE 新到达的 pending 比快照新，不能被旧快照丢弃。
  if (eventsArrivedDuringRequest) {
    for (const [key, question] of Object.entries(currentPending)) {
      if (!pendingAtStart.has(key) && !resolvedCallIds[key]) {
        pending[key] = question
      }
    }
  }

  return {
    pendingAskUserQuestions: pending,
    resolvedAskUserCallIds: {},
  }
}

function sameControlState(
  left: ConversationControlState | null,
  right: ConversationControlState
): boolean {
  return (
    left !== null &&
    left.phase === right.phase &&
    left.canSubmitPrompt === right.canSubmitPrompt &&
    left.canRequestCompact === right.canRequestCompact &&
    left.compactPending === right.compactPending &&
    left.compacting === right.compacting &&
    left.activeTurnId === right.activeTurnId
  )
}

function sameAgentSession(
  left: AgentSessionLink,
  right: AgentSessionLink
): boolean {
  return (
    left.childSessionId === right.childSessionId &&
    left.toolCallId === right.toolCallId &&
    left.agentName === right.agentName &&
    left.task === right.task &&
    left.status === right.status &&
    left.finalSessionId === right.finalSessionId &&
    left.summary === right.summary &&
    left.error === right.error &&
    left.phase === right.phase &&
    left.currentTool === right.currentTool
  )
}

function updateToolCall(
  blocks: ConversationBlock[],
  blockId: string,
  update: (
    block: Extract<ConversationBlock, { kind: 'toolCall' }>
  ) => Extract<ConversationBlock, { kind: 'toolCall' }>
): ConversationBlock[] {
  const index = blocks.findIndex(
    (block) => block.kind === 'toolCall' && block.id === blockId
  )
  if (index === -1) return blocks

  const block = blocks[index]
  if (block.kind !== 'toolCall') return blocks

  const next = [...blocks]
  next[index] = update(block)
  return next
}

/**
 * Applies one animation frame worth of conversation changes without notifying
 * subscribers between individual deltas.
 */
export function reduceConversationDeltas(
  current: ConversationRenderState,
  deltas: ConversationDelta[],
  cursor?: string | null
): ConversationRenderPatch {
  let blocks = current.blocks
  let control = current.control
  let phase = current.phase
  let agentSessions = current.agentSessions
  let statusItems = current.statusItems
  let statusItemRevisions = current.statusItemRevisions
  let pendingAskUserQuestions = current.pendingAskUserQuestions
  let resolvedAskUserCallIds = current.resolvedAskUserCallIds
  let askUserEventRevision = current.askUserEventRevision
  let transientHint = current.transientHint
  let pendingBlockDeltas: BlockDelta[] = []

  const flushBlockDeltas = () => {
    if (pendingBlockDeltas.length === 0) return
    blocks = applyCoalescedDeltas(blocks, pendingBlockDeltas)
    pendingBlockDeltas = []
  }

  for (const coalesced of coalesceDeltas(deltas)) {
    if (coalesced.kind !== 'other') {
      pendingBlockDeltas.push(coalesced)
      continue
    }

    flushBlockDeltas()
    const delta = coalesced.delta
    switch (delta.kind) {
      case 'appendBlock': {
        const baseBlocks =
          delta.block.kind === 'compactSummary'
            ? blocks.filter((block) => block.kind !== 'compactSummary')
            : blocks
        blocks = upsertBlock(baseBlocks, delta.block)
        break
      }

      case 'finalizeBlock':
        blocks = upsertBlock(blocks, delta.block)
        break

      case 'updateControlState': {
        if (!sameControlState(control, delta.control)) {
          control = delta.control
        }
        phase = resolvePhase(delta.control, current.compactSubmitting)
        break
      }

      case 'agentSessionUpdated': {
        const incoming = delta.agentSession
        const index = agentSessions.findIndex(
          (session) => session.childSessionId === incoming.childSessionId
        )
        if (index === -1) {
          agentSessions = [...agentSessions, incoming]
          break
        }

        const merged = mergeAgentSession(agentSessions[index], incoming)
        if (sameAgentSession(agentSessions[index], merged)) break

        const next = [...agentSessions]
        next[index] = merged
        agentSessions = next
        break
      }

      case 'agentSessionRemoved': {
        const next = agentSessions.filter(
          (session) => session.childSessionId !== delta.childSessionId
        )
        if (next.length !== agentSessions.length) {
          agentSessions = next
        }
        break
      }

      case 'statusItemUpdate': {
        const valueChanged = delta.text
          ? statusItems[delta.id] !== delta.text
          : statusItems[delta.id] !== undefined
        if (valueChanged) {
          const next = { ...statusItems }
          if (delta.text) {
            next[delta.id] = delta.text
          } else {
            delete next[delta.id]
          }
          statusItems = next
        }
        statusItemRevisions = {
          ...statusItemRevisions,
          [delta.id]: (statusItemRevisions[delta.id] ?? 0) + 1,
        }
        break
      }

      case 'extensionRegistryChanged':
        transientHint = '扩展已更新'
        break

      case 'extensionEvent': {
        if (delta.extensionId !== 'astrcode-ask-user') break
        if (delta.eventType === 'ask_user.pending') {
          try {
            const pending = decodePendingAskUserQuestion(delta.payload)
            const key = pendingAskUserKey(pending.sessionId, pending.callId)
            if (pendingAskUserQuestions[key]) {
              break
            }
            if (resolvedAskUserCallIds[key]) {
              const nextResolved = { ...resolvedAskUserCallIds }
              delete nextResolved[key]
              resolvedAskUserCallIds = nextResolved
            }
            pendingAskUserQuestions = {
              ...pendingAskUserQuestions,
              [key]: pending,
            }
            askUserEventRevision += 1
          } catch (error) {
            console.warn('Ignoring invalid ask-user pending event', error)
          }
          break
        }
        if (delta.eventType === 'ask_user.resolved') {
          if (
            typeof delta.payload !== 'object' ||
            delta.payload === null ||
            Array.isArray(delta.payload)
          ) {
            break
          }
          const payload = delta.payload as Record<string, unknown>
          const sessionId = payload.sessionId
          const callId = payload.callId
          if (typeof sessionId !== 'string' || typeof callId !== 'string') {
            break
          }
          const key = pendingAskUserKey(sessionId, callId)
          const wasPending = pendingAskUserQuestions[key] !== undefined
          const trackResolution = current.pendingAskUserRefreshInFlight
          if (
            !wasPending &&
            (!trackResolution || resolvedAskUserCallIds[key] === sessionId)
          ) {
            break
          }
          if (wasPending) {
            const nextPending = { ...pendingAskUserQuestions }
            delete nextPending[key]
            pendingAskUserQuestions = nextPending
          }
          if (trackResolution) {
            resolvedAskUserCallIds = {
              ...resolvedAskUserCallIds,
              [key]: sessionId,
            }
          }
          askUserEventRevision += 1
        }
        break
      }

      case 'toolApprovalRequested':
        blocks = updateToolCall(blocks, delta.approval.callId, (block) => ({
          ...block,
          approval: delta.approval,
        }))
        break

      case 'toolApprovalResolved':
        blocks = updateToolCall(blocks, delta.callId, (block) => {
          const next = { ...block }
          delete next.approval
          return next
        })
        break

      case 'rehydrateRequired':
      case 'sessionContinued':
        break

      case 'patchBlock':
      case 'thinkingDelta':
      case 'patchArguments':
      case 'toolOutput':
        // These variants were converted to BlockDelta by coalesceDeltas.
        break
    }
  }

  flushBlockDeltas()

  const patch: ConversationRenderPatch = {}
  if (blocks !== current.blocks) patch.blocks = blocks
  if (control !== current.control) patch.control = control
  if (phase !== current.phase) patch.phase = phase
  if (agentSessions !== current.agentSessions) {
    patch.agentSessions = agentSessions
  }
  if (statusItems !== current.statusItems) patch.statusItems = statusItems
  if (statusItemRevisions !== current.statusItemRevisions) {
    patch.statusItemRevisions = statusItemRevisions
  }
  if (pendingAskUserQuestions !== current.pendingAskUserQuestions) {
    patch.pendingAskUserQuestions = pendingAskUserQuestions
  }
  if (resolvedAskUserCallIds !== current.resolvedAskUserCallIds) {
    patch.resolvedAskUserCallIds = resolvedAskUserCallIds
  }
  if (askUserEventRevision !== current.askUserEventRevision) {
    patch.askUserEventRevision = askUserEventRevision
  }
  if (transientHint !== current.transientHint) {
    patch.transientHint = transientHint
  }
  if (cursor !== undefined && cursor !== current.cursor) {
    patch.cursor = cursor
  }
  return patch
}

export function applyDeltasToState(
  deltas: ConversationDelta[],
  get: () => AppState,
  set: (
    partial: Partial<AppState> | ((current: AppState) => Partial<AppState>)
  ) => void,
  cursor?: string | null
): void {
  if (deltas.length === 0 && cursor === undefined) return

  set((current) => {
    const patch = reduceConversationDeltas(current, deltas, cursor)
    return Object.keys(patch).length > 0 ? patch : current
  })
  applyConversationDeltaEffects(deltas, get)
}

export function applyDeltaToState(
  delta: ConversationDelta,
  get: () => AppState,
  set: (
    partial: Partial<AppState> | ((current: AppState) => Partial<AppState>)
  ) => void
): void {
  applyDeltasToState([delta], get, set)
}
