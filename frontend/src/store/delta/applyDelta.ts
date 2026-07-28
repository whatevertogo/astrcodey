import type {
  AgentSessionLink,
  ConversationBlock,
  ConversationControlState,
  ConversationDelta,
} from '../../services/types'
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
  | 'transientHint'
>

type ConversationRenderPatch = Partial<ConversationRenderState>

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
        if (
          (delta.text && statusItems[delta.id] === delta.text) ||
          (!delta.text && statusItems[delta.id] === undefined)
        ) {
          break
        }
        const next = { ...statusItems }
        if (delta.text) {
          next[delta.id] = delta.text
        } else {
          delete next[delta.id]
        }
        statusItems = next
        break
      }

      case 'extensionRegistryChanged':
        transientHint = '扩展已更新'
        break

      case 'patchToolMetadata':
        blocks = updateToolCall(blocks, delta.blockId, (block) => ({
          ...block,
          metadata: {
            ...(block.metadata ?? {}),
            ...delta.metadata,
            toolGateApproval: {
              ...((block.metadata?.toolGateApproval as
                | Record<string, unknown>
                | undefined) ?? {}),
              ...((delta.metadata.toolGateApproval as
                | Record<string, unknown>
                | undefined) ?? {}),
            },
          },
        }))
        break

      case 'patchToolCall':
        blocks = updateToolCall(blocks, delta.blockId, (block) => {
          const metadata = delta.metadata
            ? {
                ...(block.metadata ?? {}),
                ...delta.metadata,
              }
            : block.metadata
          return {
            ...block,
            text: delta.text,
            ...(metadata ? { metadata } : {}),
          }
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
