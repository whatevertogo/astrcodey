import { create } from 'zustand'
import * as api from '../services/api'
import { consumeSseStream } from '../services/sse-stream'
import { getHostBridge } from '../lib/hostBridge'
import type {
  AgentSessionLink,
  ConversationBlock,
  ConversationControlState,
  ConversationDelta,
  SessionListItem,
  Phase,
} from '../services/types'

interface ConversationState {
  serverPort: number | null
  connectionStatus: 'disconnected' | 'connecting' | 'connected' | 'error'
  connectionError: string | null

  sessions: SessionListItem[]
  activeSessionId: string | null
  activeSessionTitle: string | null
  workingDir: string | null

  blocks: ConversationBlock[]
  control: ConversationControlState | null
  cursor: string | null
  phase: Phase
  compactSubmitting: boolean

  streamAbortController: AbortController | null
  modelRefreshKey: number
  agentSessions: AgentSessionLink[]
  statusItems: Record<string, string>

  initServer: () => Promise<void>
  refreshSessions: () => Promise<void>
  createSession: (workingDir: string) => Promise<void>
  deleteSession: (sessionId: string) => Promise<void>
  deleteProject: (workingDir: string) => Promise<void>
  bumpModelRefreshKey: () => void
  switchSession: (sessionId: string) => Promise<void>
  submitPrompt: (text: string) => Promise<boolean>
  abortCurrentTurn: () => Promise<void>
  applyDelta: (delta: ConversationDelta) => void
}

function phaseFromControl(control: ConversationControlState | null): Phase {
  return control?.phase ?? 'idle'
}

function mergeBlock(
  current: ConversationBlock,
  incoming: ConversationBlock
): ConversationBlock {
  if (current.kind === 'assistant' && incoming.kind === 'assistant') {
    return {
      ...incoming,
      text: incoming.text ?? current.text,
      reasoningContent: incoming.reasoningContent ?? current.reasoningContent,
    }
  }

  if (current.kind === 'toolCall' && incoming.kind === 'toolCall') {
    return {
      ...incoming,
      name: incoming.name ?? current.name,
      arguments: incoming.arguments.trim()
        ? incoming.arguments
        : current.arguments,
      text: incoming.text ?? current.text,
      // taskId 不随 FinalizeBlock 返回，保留当前值
      taskId: incoming.taskId ?? current.taskId,
      metadata: incoming.metadata ?? current.metadata,
    }
  }

  return incoming
}

function upsertBlock(
  blocks: ConversationBlock[],
  block: ConversationBlock
): ConversationBlock[] {
  const idx = blocks.findIndex((item) => item.id === block.id)
  if (idx === -1) return [...blocks, block]

  const next = [...blocks]
  next[idx] = mergeBlock(next[idx], block)
  return next
}

function commandNoteBlock(message: string): ConversationBlock {
  return {
    kind: 'systemNote',
    id: `command-${Date.now()}`,
    text: message,
  }
}

function isCompactCommand(text: string): boolean {
  return /^\/compact(?:\s|$)/.test(text.trim())
}

function patchAssistantBlock(
  blocks: ConversationBlock[],
  blockId: string,
  textDelta: string
): ConversationBlock[] {
  if (!blockId || !textDelta) return blocks

  const idx = blocks.findIndex((block) => block.id === blockId)
  if (idx === -1) {
    return [
      ...blocks,
      {
        kind: 'assistant',
        id: blockId,
        text: textDelta,
        status: 'streaming',
      },
    ]
  }

  const block = blocks[idx]
  if (block.kind !== 'assistant' && block.kind !== 'toolCall') {
    return blocks
  }

  const next = [...blocks]
  next[idx] = { ...block, text: (block.text ?? '') + textDelta }
  return next
}

function patchAssistantThinking(
  blocks: ConversationBlock[],
  blockId: string,
  delta: string
): ConversationBlock[] {
  if (!blockId || !delta) return blocks

  const idx = blocks.findIndex((block) => block.id === blockId)
  if (idx === -1) {
    return [
      ...blocks,
      {
        kind: 'assistant',
        id: blockId,
        text: '',
        reasoningContent: delta,
        status: 'streaming',
      },
    ]
  }

  const block = blocks[idx]
  if (block.kind !== 'assistant') return blocks

  const next = [...blocks]
  next[idx] = {
    ...block,
    reasoningContent: (block.reasoningContent ?? '') + delta,
  }
  return next
}

export const useAppStore = create<ConversationState>((set, get) => ({
  serverPort: null,
  connectionStatus: 'disconnected',
  connectionError: null,
  sessions: [],
  activeSessionId: null,
  activeSessionTitle: null,
  workingDir: null,
  blocks: [],
  control: null,
  cursor: null,
  phase: 'idle',
  compactSubmitting: false,
  streamAbortController: null,
  modelRefreshKey: 0,
  agentSessions: [],
  statusItems: {},

  initServer: async () => {
    set({ connectionStatus: 'connecting', connectionError: null })

    const bridge = getHostBridge()

    if (bridge.isDesktopHost) {
      try {
        const { invoke } = await import('@tauri-apps/api/core')
        const result = await invoke<{ port: number; token?: string }>(
          'start_server'
        )
        api.setServerPort(result.port, result.token)
        set({ serverPort: result.port })
      } catch (err) {
        set({
          connectionStatus: 'error',
          connectionError: err instanceof Error ? err.message : String(err),
        })
        return
      }
    } else {
      api.initBaseUrl()
      const envToken = (
        import.meta as unknown as { env: Record<string, string> }
      ).env?.VITE_AUTH_TOKEN
      if (envToken) {
        api.setAuthToken(envToken)
      }
    }

    set({ connectionStatus: 'connected' })
    await get().refreshSessions()
  },

  refreshSessions: async () => {
    try {
      const response = await api.listSessions()
      set({ sessions: response.sessions })
    } catch (err) {
      console.error('Failed to refresh sessions:', err)
    }
  },

  createSession: async (workingDir: string) => {
    const response = await api.createSession(workingDir)
    await get().refreshSessions()
    await get().switchSession(response.sessionId)
  },

  deleteSession: async (sessionId: string) => {
    try {
      await api.deleteSession(sessionId)
    } catch (err) {
      console.error('Failed to delete session:', err)
    }
    const state = get()
    if (state.activeSessionId === sessionId) {
      state.streamAbortController?.abort()
      set({
        activeSessionId: null,
        activeSessionTitle: null,
        blocks: [],
        control: null,
        cursor: null,
        phase: 'idle',
        compactSubmitting: false,
        workingDir: null,
        agentSessions: [],
      })
    }
    await get().refreshSessions()
  },

  deleteProject: async (workingDir: string) => {
    try {
      await api.deleteProject(workingDir)
    } catch (err) {
      console.error('Failed to delete project:', err)
    }
    const state = get()
    const activeSession = state.sessions.find(
      (s) => s.sessionId === state.activeSessionId
    )
    if (activeSession && activeSession.workingDir === workingDir) {
      state.streamAbortController?.abort()
      set({
        activeSessionId: null,
        activeSessionTitle: null,
        blocks: [],
        control: null,
        cursor: null,
        phase: 'idle',
        compactSubmitting: false,
        workingDir: null,
        agentSessions: [],
      })
    }
    await get().refreshSessions()
  },

  bumpModelRefreshKey: () => {
    set((s) => ({ modelRefreshKey: s.modelRefreshKey + 1 }))
  },

  switchSession: async (sessionId: string) => {
    const state = get()

    state.streamAbortController?.abort()

    set({
      activeSessionId: sessionId,
      blocks: [],
      control: null,
      cursor: null,
      phase: 'idle',
      compactSubmitting: false,
      agentSessions: [],
    })

    try {
      const snapshot = await api.getConversation(sessionId)
      const sessions = get().sessions
      const sessionItem = sessions.find((s) => s.sessionId === sessionId)

      set({
        blocks: snapshot.blocks,
        control: snapshot.control,
        cursor: snapshot.cursor.value,
        phase: phaseFromControl(snapshot.control),
        activeSessionTitle: snapshot.sessionTitle,
        workingDir: sessionItem?.workingDir ?? null,
        agentSessions: snapshot.agentSessions ?? [],
      })

      connectSse(sessionId, snapshot.cursor.value, get, set)
    } catch (err) {
      console.error('Failed to switch session:', err)
    }
  },

  submitPrompt: async (text: string) => {
    const { activeSessionId, control } = get()
    if (!activeSessionId || !control?.canSubmitPrompt) {
      return false
    }

    const compactCommand = isCompactCommand(text)
    if (compactCommand) {
      set({ compactSubmitting: true, phase: 'compacting' })
    }

    try {
      const response = await api.submitPrompt(activeSessionId, text)
      if (response.kind === 'handled') {
        if (get().activeSessionId !== response.sessionId) {
          return true
        }
        if (response.message === 'compact accepted') {
          await get().refreshSessions()
          await get().switchSession(response.sessionId)
        } else if (response.message.trim()) {
          set((current) => ({
            blocks: [...current.blocks, commandNoteBlock(response.message)],
          }))
        }
      }
      return true
    } finally {
      if (compactCommand) {
        const current = get()
        set({
          compactSubmitting: false,
          phase: phaseFromControl(current.control),
        })
      }
    }
  },

  abortCurrentTurn: async () => {
    const { activeSessionId } = get()
    if (!activeSessionId) return

    await api.abortSession(activeSessionId)
  },

  applyDelta: (delta: ConversationDelta) => {
    const state = get()

    switch (delta.kind) {
      case 'appendBlock':
        set((current) => ({
          blocks: upsertBlock(current.blocks, delta.block),
        }))
        // 新用户消息到达时刷新侧边栏标题
        if (delta.block.kind === 'user') {
          void get().refreshSessions()
        }
        break

      case 'patchBlock':
        set((current) => ({
          blocks: patchAssistantBlock(
            current.blocks,
            delta.blockId,
            delta.textDelta
          ),
        }))
        break

      case 'finalizeBlock':
        set((current) => ({
          blocks: upsertBlock(current.blocks, delta.block),
        }))
        break

      case 'updateControlState':
        set({
          control: delta.control,
          phase: get().compactSubmitting
            ? 'compacting'
            : phaseFromControl(delta.control),
        })
        break

      case 'thinkingDelta':
        set((current) => ({
          blocks: patchAssistantThinking(
            current.blocks,
            delta.blockId,
            delta.delta
          ),
        }))
        break

      case 'patchArguments': {
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
          next[idx] = { ...block, arguments: argumentsText }
          return { blocks: next }
        })
        break
      }

      case 'toolOutput': {
        set((current) => {
          const idx = current.blocks.findIndex(
            (b) => b.kind === 'toolCall' && b.id === delta.callId
          )
          if (idx === -1) return {}
          const block = current.blocks[idx]
          if (block.kind !== 'toolCall') return {}
          const prefix = delta.stream === 'stderr' ? '\n[stderr] ' : '\n'
          const next = [...current.blocks]
          next[idx] = { ...block, text: block.text + prefix + delta.delta }
          return { blocks: next }
        })
        break
      }

      case 'rehydrateRequired': {
        const sessionId = state.activeSessionId
        if (sessionId) {
          void get().switchSession(sessionId)
        }
        break
      }

      case 'sessionContinued': {
        void get().refreshSessions()
        // Same-session compact: newSessionId == parentSessionId.
        // switchSession clears blocks and reloads from API, which returns
        // the compacted conversation snapshot — effectively refreshing the view.
        void get().switchSession(delta.newSessionId)
        break
      }

      case 'toolCallBackgrounded': {
        set((current) => {
          const idx = current.blocks.findIndex(
            (b) => b.kind === 'toolCall' && b.id === delta.callId
          )
          if (idx === -1) return {}
          const block = current.blocks[idx]
          if (block.kind !== 'toolCall') return {}
          const next = [...current.blocks]
          next[idx] = {
            ...block,
            text: `Task moved to background (task: ${delta.taskId}). Result will arrive when done.`,
            status: 'backgrounded',
            taskId: delta.taskId,
          }
          return { blocks: next }
        })
        break
      }

      case 'agentSessionUpdated': {
        set((current) => {
          const incoming = delta.agentSession
          const idx = current.agentSessions.findIndex(
            (s) => s.childSessionId === incoming.childSessionId
          )
          if (idx === -1) {
            return { agentSessions: [...current.agentSessions, incoming] }
          }
          const next = [...current.agentSessions]
          next[idx] = {
            ...next[idx],
            status: incoming.status,
          }
          return { agentSessions: next }
        })
        break
      }
    }
  },
}))

const SSE_RECONNECT_DELAY_MS = 3000

function connectSse(
  sessionId: string,
  cursor: string,
  get: () => ConversationState,
  set: (
    partial:
      | Partial<ConversationState>
      | ((s: ConversationState) => Partial<ConversationState>)
  ) => void
): void {
  const abortController = new AbortController()
  set({ streamAbortController: abortController })

  // rAF batcher: collect high-frequency deltas and flush once per animation frame.
  const pendingDeltas: ConversationDelta[] = []
  let latestCursor: string | null = null
  let rafId: number | null = null
  let timeoutId: number | null = null

  const flushPending = () => {
    if (rafId !== null) {
      cancelAnimationFrame(rafId)
      rafId = null
    }
    if (timeoutId !== null) {
      clearTimeout(timeoutId)
      timeoutId = null
    }

    // Always commit cursor regardless of pending deltas.
    if (latestCursor !== null) {
      set({ cursor: latestCursor })
      latestCursor = null
    }

    if (pendingDeltas.length === 0) return

    const deltas = pendingDeltas.splice(0)
    for (const delta of deltas) {
      get().applyDelta(delta)
    }
  }

  const scheduleFlush = () => {
    if (rafId === null) {
      rafId = requestAnimationFrame(flushPending)
    }
    if (timeoutId === null) {
      timeoutId = window.setTimeout(flushPending, 32)
    }
  }

  const isDeferrable = (delta: ConversationDelta): boolean =>
    delta.kind === 'patchBlock' ||
    delta.kind === 'thinkingDelta' ||
    delta.kind === 'patchArguments' ||
    delta.kind === 'toolOutput'

  consumeSseStream(
    sessionId,
    cursor,
    (envelope) => {
      const current = get()
      if (current.activeSessionId !== sessionId) return
      if (envelope.cursor) {
        latestCursor = envelope.cursor.value
      }
      if (isDeferrable(envelope.delta)) {
        pendingDeltas.push(envelope.delta)
        scheduleFlush()
      } else {
        flushPending()
        get().applyDelta(envelope.delta)
      }
    },
    abortController.signal
  )
    .then((result) => {
      if (abortController.signal.aborted) return
      if (result === 'ended') {
        const current = get()
        if (current.activeSessionId === sessionId) {
          const latestCursor = current.cursor ?? cursor
          setTimeout(() => {
            if (get().activeSessionId === sessionId) {
              connectSse(sessionId, latestCursor, get, set)
            }
          }, SSE_RECONNECT_DELAY_MS)
        }
      }
    })
    .catch((err) => {
      if (abortController.signal.aborted) return
      console.error(
        'SSE stream error, reconnecting in',
        SSE_RECONNECT_DELAY_MS,
        'ms:',
        err
      )
      if (get().activeSessionId === sessionId) {
        const latestCursor = get().cursor ?? cursor
        setTimeout(() => {
          if (get().activeSessionId === sessionId) {
            connectSse(sessionId, latestCursor, get, set)
          }
        }, SSE_RECONNECT_DELAY_MS)
      }
    })
}
