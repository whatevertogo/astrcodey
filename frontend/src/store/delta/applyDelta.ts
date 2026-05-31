import type { ConversationDelta } from '../../services/types'
import type { AppState } from '../types'
import { mergeAgentSession, resolvePhase, upsertBlock } from './blockHelpers'

export function applyDeltaToState(
  state: AppState,
  delta: ConversationDelta,
  get: () => AppState,
  set: (
    partial: Partial<AppState> | ((current: AppState) => Partial<AppState>)
  ) => void
): void {
  switch (delta.kind) {
    case 'appendBlock':
      set((current) => {
        const baseBlocks =
          delta.block.kind === 'compactSummary'
            ? current.blocks.filter((b) => b.kind !== 'compactSummary')
            : current.blocks
        return {
          blocks: upsertBlock(baseBlocks, delta.block),
        }
      })
      if (delta.block.kind === 'user') {
        void get().refreshSessions()
      }
      break

    case 'patchBlock': {
      const blockId = delta.blockId
      const textDelta = delta.textDelta
      if (!blockId || !textDelta) break
      set((current) => {
        const idx = current.blocks.findIndex((b) => b.id === blockId)
        if (idx === -1) {
          return {
            blocks: [
              ...current.blocks,
              {
                kind: 'assistant',
                id: blockId,
                text: textDelta,
                status: 'streaming',
              },
            ],
          }
        }
        const block = current.blocks[idx]
        if (block.kind !== 'assistant' && block.kind !== 'toolCall') return {}
        const next = [...current.blocks]
        next[idx] = { ...block, text: (block.text ?? '') + textDelta }
        return { blocks: next }
      })
      break
    }

    case 'finalizeBlock':
      set((current) => ({
        blocks: upsertBlock(current.blocks, delta.block),
      }))
      break

    case 'updateControlState':
      set({
        control: delta.control,
        phase: resolvePhase(delta.control, get().compactSubmitting),
      })
      break

    case 'thinkingDelta': {
      const blockId = delta.blockId
      const thinkDelta = delta.delta
      if (!blockId || !thinkDelta) break
      set((current) => {
        const idx = current.blocks.findIndex((b) => b.id === blockId)
        if (idx === -1) {
          return {
            blocks: [
              ...current.blocks,
              {
                kind: 'assistant',
                id: blockId,
                text: '',
                reasoningContent: thinkDelta,
                status: 'streaming',
              },
            ],
          }
        }
        const block = current.blocks[idx]
        if (block.kind !== 'assistant') return {}
        const next = [...current.blocks]
        next[idx] = {
          ...block,
          reasoningContent: (block.reasoningContent ?? '') + thinkDelta,
        }
        return { blocks: next }
      })
      break
    }

    case 'patchArguments':
      set((current) => {
        const argumentsText = delta.arguments.trim()
        if (!argumentsText) return {}
        const idx = current.blocks.findIndex(
          (b) => b.kind === 'toolCall' && b.id === delta.blockId
        )
        if (idx === -1) return {}
        const block = current.blocks[idx]
        if (block.kind !== 'toolCall') return {}
        const next = [...current.blocks]
        next[idx] = {
          ...block,
          arguments: argumentsText,
          ...(delta.argumentsJson
            ? {
                argumentsJson: delta.argumentsJson as Record<string, unknown>,
              }
            : {}),
        }
        return { blocks: next }
      })
      break

    case 'toolOutput':
      set((current) => {
        const rawPrefix = delta.stream === 'stderr' ? '\n[stderr] ' : '\n'
        const idx = current.blocks.findIndex(
          (b) => b.kind === 'toolCall' && b.id === delta.callId
        )
        const existingText =
          idx !== -1 && current.blocks[idx].kind === 'toolCall'
            ? current.blocks[idx].text
            : ''
        const chunk =
          rawPrefix.startsWith('\n') && !existingText
            ? rawPrefix.slice(1) + delta.delta
            : rawPrefix + delta.delta
        if (idx === -1) {
          return {
            blocks: [
              ...current.blocks,
              {
                kind: 'toolCall',
                id: delta.callId,
                name: '',
                arguments: '',
                text: chunk,
                status: 'streaming',
              },
            ],
          }
        }
        const block = current.blocks[idx]
        if (block.kind !== 'toolCall') return {}
        const next = [...current.blocks]
        next[idx] = { ...block, text: block.text + chunk }
        return { blocks: next }
      })
      break

    case 'rehydrateRequired': {
      const sessionId = state.activeSessionId
      if (sessionId) {
        void get().switchSession(sessionId)
      }
      break
    }

    case 'sessionContinued':
      void get().refreshSessions()
      if (delta.newSessionId === delta.parentSessionId) {
        void get().refreshConversationSnapshot()
      } else {
        void get().switchSession(delta.newSessionId)
      }
      break

    case 'agentSessionUpdated':
      set((current) => {
        const incoming = delta.agentSession
        const idx = current.agentSessions.findIndex(
          (s) => s.childSessionId === incoming.childSessionId
        )
        if (idx === -1) {
          return { agentSessions: [...current.agentSessions, incoming] }
        }
        const next = [...current.agentSessions]
        next[idx] = mergeAgentSession(next[idx], incoming)
        return { agentSessions: next }
      })
      break

    case 'agentSessionRemoved':
      set((current) => ({
        agentSessions: current.agentSessions.filter(
          (s) => s.childSessionId !== delta.childSessionId
        ),
      }))
      break

    case 'statusItemUpdate':
      set((current) => {
        const next = { ...current.statusItems }
        if (delta.text) {
          next[delta.id] = delta.text
        } else {
          delete next[delta.id]
        }
        return { statusItems: next }
      })
      break

    case 'extensionRegistryChanged':
      set({ transientHint: '扩展已更新' })
      void get().refreshExtensionData()
      break

    case 'patchToolMetadata': {
      const { blockId, metadata } = delta
      set((current) => {
        const idx = current.blocks.findIndex((b) => b.id === blockId)
        if (idx === -1) return {}
        const block = current.blocks[idx]
        if (block.kind !== 'toolCall') return {}
        const merged = {
          ...(block.metadata ?? {}),
          ...metadata,
          toolGateApproval: {
            ...((block.metadata?.toolGateApproval as
              | Record<string, unknown>
              | undefined) ?? {}),
            ...((metadata.toolGateApproval as
              | Record<string, unknown>
              | undefined) ?? {}),
          },
        }
        const next = [...current.blocks]
        next[idx] = { ...block, metadata: merged }
        return { blocks: next }
      })
      break
    }

    case 'patchToolCall': {
      const { blockId, text, metadata } = delta
      set((current) => {
        const idx = current.blocks.findIndex((b) => b.id === blockId)
        if (idx === -1) return {}
        const block = current.blocks[idx]
        if (block.kind !== 'toolCall') return {}
        const mergedMetadata = metadata
          ? {
              ...(block.metadata ?? {}),
              ...metadata,
            }
          : block.metadata
        const next = [...current.blocks]
        next[idx] = {
          ...block,
          text,
          ...(mergedMetadata ? { metadata: mergedMetadata } : {}),
        }
        return { blocks: next }
      })
      break
    }
  }
}
