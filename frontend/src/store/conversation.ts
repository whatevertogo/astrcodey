import { create } from 'zustand'
import * as api from '../services/api'
import { consumeSseStream } from '../services/sse-stream'
import { resolveHostBridge } from '../lib/hostBridge'
import type {
  AgentSessionLink,
  AgentSessionStatus,
  ConversationBlock,
  ConversationControlState,
  ConversationDelta,
  PromptAttachment,
  SessionListItem,
  Phase,
  ToolOutputStream,
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
  transientHint: string | null
  queuedMessages: string[]

  initServer: () => Promise<void>
  refreshSessions: () => Promise<void>
  createSession: (workingDir: string) => Promise<void>
  deleteSession: (sessionId: string) => Promise<void>
  deleteProject: (workingDir: string) => Promise<void>
  bumpModelRefreshKey: () => void
  switchSession: (sessionId: string) => Promise<void>
  refreshConversationSnapshot: () => Promise<void>
  submitPrompt: (
    text: string,
    attachments?: PromptAttachment[]
  ) => Promise<boolean>
  abortCurrentTurn: () => Promise<void>
  applyDelta: (delta: ConversationDelta) => void
}

async function withTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number,
  message: string
): Promise<T> {
  let timeoutId: ReturnType<typeof setTimeout> | undefined
  const timeout = new Promise<never>((_, reject) => {
    timeoutId = setTimeout(() => reject(new Error(message)), timeoutMs)
  })
  try {
    return await Promise.race([promise, timeout])
  } finally {
    if (timeoutId) clearTimeout(timeoutId)
  }
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
      // argumentsJson 不随 FinalizeBlock 返回，保留当前值
      argumentsJson: incoming.argumentsJson ?? current.argumentsJson,
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

function isTerminalAgentStatus(
  status: AgentSessionStatus | undefined
): boolean {
  return status === 'completed' || status === 'failed'
}

function mergeAgentSession(
  current: AgentSessionLink,
  incoming: AgentSessionLink
): AgentSessionLink {
  const status =
    incoming.status ??
    (isTerminalAgentStatus(current.status) ? current.status : 'running')
  const running = status === 'running'
  const phaseProvided = incoming.phase !== undefined
  const currentTool = running
    ? phaseProvided
      ? incoming.currentTool
      : (incoming.currentTool ?? current.currentTool)
    : undefined

  return {
    ...current,
    ...incoming,
    status,
    agentName: incoming.agentName?.trim()
      ? incoming.agentName
      : current.agentName,
    task: incoming.task?.trim() ? incoming.task : current.task,
    toolCallId: incoming.toolCallId ?? current.toolCallId,
    finalSessionId: incoming.finalSessionId ?? current.finalSessionId,
    summary: incoming.summary ?? current.summary,
    error: incoming.error ?? current.error,
    phase: running ? (incoming.phase ?? current.phase) : undefined,
    currentTool,
  }
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

// ── Delta coalescing: merge same-block deltas before applying ──

type CoalescedDelta =
  | { kind: 'patchBlock'; blockId: string; textDelta: string }
  | { kind: 'thinkingDelta'; blockId: string; delta: string }
  | {
      kind: 'patchArguments'
      blockId: string
      arguments: string
      argumentsJson?: Record<string, unknown>
    }
  | {
      kind: 'toolOutput'
      callId: string
      parts: { stream: ToolOutputStream; delta: string }[]
    }
  | { kind: 'other'; delta: ConversationDelta }

function coalesceDeltas(deltas: ConversationDelta[]): CoalescedDelta[] {
  const textPatches = new Map<string, string>()
  const thinkingPatches = new Map<string, string>()
  const argPatches = new Map<
    string,
    { arguments: string; argumentsJson?: Record<string, unknown> }
  >()
  const toolOutputs = new Map<
    string,
    { stream: ToolOutputStream; delta: string }[]
  >()
  const others: CoalescedDelta[] = []

  for (const delta of deltas) {
    switch (delta.kind) {
      case 'patchBlock': {
        const existing = textPatches.get(delta.blockId)
        textPatches.set(
          delta.blockId,
          existing ? existing + delta.textDelta : delta.textDelta
        )
        break
      }
      case 'thinkingDelta': {
        const existing = thinkingPatches.get(delta.blockId)
        thinkingPatches.set(
          delta.blockId,
          existing ? existing + delta.delta : delta.delta
        )
        break
      }
      case 'patchArguments': {
        argPatches.set(delta.blockId, {
          arguments: delta.arguments,
          argumentsJson: delta.argumentsJson,
        })
        break
      }
      case 'toolOutput': {
        const existing = toolOutputs.get(delta.callId)
        if (existing) {
          existing.push({ stream: delta.stream, delta: delta.delta })
        } else {
          toolOutputs.set(delta.callId, [
            { stream: delta.stream, delta: delta.delta },
          ])
        }
        break
      }
      default:
        others.push({ kind: 'other', delta })
    }
  }

  const result: CoalescedDelta[] = []

  for (const [blockId, textDelta] of textPatches) {
    result.push({ kind: 'patchBlock', blockId, textDelta })
  }
  for (const [blockId, delta] of thinkingPatches) {
    result.push({ kind: 'thinkingDelta', blockId, delta })
  }
  for (const [blockId, args] of argPatches) {
    result.push({ kind: 'patchArguments', blockId, ...args })
  }
  for (const [callId, parts] of toolOutputs) {
    result.push({ kind: 'toolOutput', callId, parts })
  }
  result.push(...others)

  return result
}

/** Apply all coalesced deltas to blocks in a single array pass. */
function applyCoalescedDeltas(
  blocks: ConversationBlock[],
  coalesced: CoalescedDelta[],
  queuedMessages: string[]
): { blocks: ConversationBlock[]; queuedMessages: string[] } {
  if (coalesced.length === 0) return { blocks, queuedMessages }

  // Collect all index mutations, then apply once
  const mutations = new Map<number, ConversationBlock>()
  let needsNewBlocks = false

  const findOrCreateIdx = (
    blockId: string,
    kind: 'assistant' | 'toolCall'
  ): number => {
    const idx = blocks.findIndex((b) => b.id === blockId)
    if (idx !== -1) return idx
    // Block not found — append a new one
    const newBlock: ConversationBlock =
      kind === 'assistant'
        ? { kind: 'assistant', id: blockId, text: '', status: 'streaming' }
        : {
            kind: 'toolCall',
            id: blockId,
            name: '',
            arguments: '',
            text: '',
            status: 'streaming',
          }
    mutations.set(blocks.length, newBlock)
    needsNewBlocks = true
    return blocks.length
  }

  for (const c of coalesced) {
    switch (c.kind) {
      case 'patchBlock': {
        const idx = findOrCreateIdx(c.blockId, 'assistant')
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'assistant' && block.kind !== 'toolCall') break
        mutations.set(idx, { ...block, text: (block.text ?? '') + c.textDelta })
        needsNewBlocks = true
        break
      }
      case 'thinkingDelta': {
        const idx = findOrCreateIdx(c.blockId, 'assistant')
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'assistant') break
        mutations.set(idx, {
          ...block,
          reasoningContent: (block.reasoningContent ?? '') + c.delta,
        })
        needsNewBlocks = true
        break
      }
      case 'patchArguments': {
        const idx = blocks.findIndex(
          (b) => b.kind === 'toolCall' && b.id === c.blockId
        )
        if (idx === -1) break
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'toolCall') break
        if (!c.arguments.trim()) break
        mutations.set(idx, {
          ...block,
          arguments: c.arguments,
          ...(c.argumentsJson ? { argumentsJson: c.argumentsJson } : {}),
        })
        needsNewBlocks = true
        break
      }
      case 'toolOutput': {
        const idx = blocks.findIndex(
          (b) => b.kind === 'toolCall' && b.id === c.callId
        )
        if (idx === -1) break
        const block = mutations.get(idx) ?? blocks[idx]
        if (block.kind !== 'toolCall') break
        const output = c.parts
          .map((p) => (p.stream === 'stderr' ? '\n[stderr] ' : '\n') + p.delta)
          .join('')
        mutations.set(idx, { ...block, text: block.text + output })
        needsNewBlocks = true
        break
      }
      case 'other': {
        // Non-coalescable deltas need the full applyDelta logic
        // They will be handled after the blocks mutation pass
        break
      }
    }
  }

  // Build the new blocks array if any mutations occurred
  let newBlocks = blocks
  if (needsNewBlocks) {
    if (mutations.has(blocks.length)) {
      // New block was appended
      newBlocks = [...blocks]
      for (const [idx, block] of mutations) {
        if (idx < blocks.length) {
          newBlocks[idx] = block
        } else {
          newBlocks.push(block)
        }
      }
    } else {
      newBlocks = [...blocks]
      for (const [idx, block] of mutations) {
        newBlocks[idx] = block
      }
    }
  }

  return { blocks: newBlocks, queuedMessages }
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
  transientHint: null,
  queuedMessages: [],

  initServer: async () => {
    set({ connectionStatus: 'connecting', connectionError: null })

    const bridge = await resolveHostBridge()

    if (bridge.isDesktopHost) {
      try {
        const { invoke } = await import('@tauri-apps/api/core')
        const result = await withTimeout(
          invoke<{ port: number; token?: string }>('start_server'),
          15_000,
          '启动 AstrCode 服务超时，请关闭残留 astrcode-http-server 进程后重试'
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
        queuedMessages: [],
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
        queuedMessages: [],
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
      transientHint: null,
      queuedMessages: [],
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

      connectSse(sessionId, snapshot.cursor.value, 0, get, set)
    } catch (err) {
      console.error('Failed to switch session:', err)
    }
  },

  refreshConversationSnapshot: async () => {
    const { activeSessionId } = get()
    if (!activeSessionId) return

    try {
      const snapshot = await api.getConversation(activeSessionId)

      set({
        blocks: snapshot.blocks,
        control: snapshot.control,
        cursor: snapshot.cursor.value,
        phase: phaseFromControl(snapshot.control),
        activeSessionTitle: snapshot.sessionTitle,
        agentSessions: snapshot.agentSessions ?? [],
      })
    } catch (err) {
      console.error('Failed to refresh conversation snapshot:', err)
    }
  },

  // TODO(web-client): 连发 prompt 时服务端返回 `handled` + `queued for next turn`；
  // 应依赖 SSE `updateControlState` / snapshot.control，勿本地猜 phase。
  // ESC 中止后 step 注入仅 TUI `SubmitPromptStep`；Web 待产品定义后再接。
  submitPrompt: async (text: string, attachments: PromptAttachment[] = []) => {
    const { activeSessionId } = get()
    if (!activeSessionId) {
      return false
    }

    const compactCommand = isCompactCommand(text)
    if (compactCommand) {
      set({ compactSubmitting: true, phase: 'compacting' })
    }

    try {
      const response = await api.submitPrompt(
        activeSessionId,
        text,
        attachments
      )
      if (response.kind === 'handled') {
        if (get().activeSessionId !== response.sessionId) {
          return true
        }
        if (response.message === 'compact accepted') {
          await get().refreshSessions()
          await get().switchSession(response.sessionId)
        } else if (response.message === 'queued for next turn') {
          set((current) => ({
            queuedMessages: [...current.queuedMessages, text],
          }))
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
        set((current) => {
          const baseBlocks =
            delta.block.kind === 'compactSummary'
              ? current.blocks.filter((b) => b.kind !== 'compactSummary')
              : current.blocks
          return {
            blocks: upsertBlock(baseBlocks, delta.block),
            queuedMessages:
              delta.block.kind === 'user' && current.queuedMessages.length > 0
                ? current.queuedMessages.slice(1)
                : current.queuedMessages,
          }
        })
        // 新用户消息到达时刷新侧边栏标题
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
          phase: get().compactSubmitting
            ? 'compacting'
            : phaseFromControl(delta.control),
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
        // Use lightweight refresh without breaking the SSE stream.
        if (delta.newSessionId === delta.parentSessionId) {
          void get().refreshConversationSnapshot()
        } else {
          void get().switchSession(delta.newSessionId)
        }
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
          next[idx] = mergeAgentSession(next[idx], incoming)
          return { agentSessions: next }
        })
        break
      }

      case 'agentSessionRemoved': {
        set((current) => ({
          agentSessions: current.agentSessions.filter(
            (s) => s.childSessionId !== delta.childSessionId
          ),
        }))
        break
      }

      case 'statusItemUpdate': {
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
      }
    }
  },
}))

const SSE_RECONNECT_BASE_MS = 1000
const SSE_RECONNECT_MAX_MS = 30_000

function sseReconnectDelayMs(attempt: number): number {
  const capped = Math.min(
    SSE_RECONNECT_MAX_MS,
    SSE_RECONNECT_BASE_MS * 2 ** attempt
  )
  const jitter = Math.random() * 0.3 * capped
  return Math.round(capped + jitter)
}

function connectSse(
  sessionId: string,
  cursor: string,
  reconnectAttempt: number,
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

    if (pendingDeltas.length === 0) {
      // Still commit cursor even if no deltas
      if (latestCursor !== null) {
        set({ cursor: latestCursor })
        latestCursor = null
      }
      return
    }

    const deltas = pendingDeltas.splice(0)
    const cursorUpdate = latestCursor !== null ? { cursor: latestCursor } : null
    latestCursor = null

    const coalesced = coalesceDeltas(deltas)

    // Separate coalescable (text) deltas from "other" deltas
    const textDeltas: CoalescedDelta[] = []
    const otherDeltas: ConversationDelta[] = []
    for (const c of coalesced) {
      if (c.kind === 'other') {
        otherDeltas.push(c.delta)
      } else {
        textDeltas.push(c)
      }
    }

    // Apply text deltas in a single set() pass
    if (textDeltas.length > 0) {
      set((current) => {
        const { blocks: newBlocks, queuedMessages } = applyCoalescedDeltas(
          current.blocks,
          textDeltas,
          current.queuedMessages
        )
        return {
          blocks: newBlocks,
          queuedMessages,
          ...(cursorUpdate ?? {}),
        }
      })
    } else if (cursorUpdate) {
      set(cursorUpdate)
    }

    // Apply non-coalescable deltas (appendBlock, finalizeBlock, etc.) individually
    for (const delta of otherDeltas) {
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
          const delayMs = sseReconnectDelayMs(reconnectAttempt)
          setTimeout(() => {
            if (get().activeSessionId === sessionId) {
              connectSse(
                sessionId,
                latestCursor,
                reconnectAttempt + 1,
                get,
                set
              )
            }
          }, delayMs)
        }
      }
    })
    .catch((err) => {
      if (abortController.signal.aborted) return
      const delayMs = sseReconnectDelayMs(reconnectAttempt)
      console.error('SSE stream error, reconnecting in', delayMs, 'ms:', err)
      if (get().activeSessionId === sessionId) {
        const latestCursor = get().cursor ?? cursor
        setTimeout(() => {
          if (get().activeSessionId === sessionId) {
            connectSse(sessionId, latestCursor, reconnectAttempt + 1, get, set)
          }
        }, delayMs)
      }
    })
}
