// Types aligned with astrcode-protocol/src/http.rs DTOs

export type Phase =
  | 'idle'
  | 'thinking'
  | 'streaming'
  | 'calling_tool'
  | 'compacting'
  | 'error'
export type ToolOutputStream = 'stdout' | 'stderr'
export type BlockStatus = 'streaming' | 'complete' | 'error' | 'backgrounded'

// ── Request/Response ──

export interface CreateSessionRequest {
  workingDir: string
}

export interface CreateSessionResponse {
  sessionId: string
}

export interface PromptRequest {
  text: string
}

export type PromptSubmitResponse =
  | {
      kind: 'accepted'
      sessionId: string
      turnId: string
      branchedFromSessionId?: string
    }
  | {
      kind: 'handled'
      sessionId: string
      message: string
    }

export interface CompactSessionResponse {
  accepted: boolean
  deferred: boolean
  newSessionId?: string
  message: string
}

export interface SlashCommandInfo {
  name: string
  description: string
  needsArgument: boolean
  source: 'builtin' | 'plugin' | 'skill' | string
}

export interface KeybindingInfo {
  key: string
  command: string
  arguments: string
  description: string
}

export interface StatusItemInfo {
  id: string
  text: string
  priority: number
}

export interface SlashCommandListResponse {
  commands: SlashCommandInfo[]
  keybindings: KeybindingInfo[]
  statusItems: StatusItemInfo[]
}

// ── Session List ──

export interface SessionListItem {
  sessionId: string
  workingDir: string
  displayName: string
  title: string
  createdAt: string
  updatedAt: string
  parentSessionId?: string
  parentStorageSeq?: number
  phase: Phase
  firstUserMessage?: string
}

export interface SessionListResponse {
  sessions: SessionListItem[]
}

// ── Conversation Snapshot ──

export type AgentSessionStatus = 'running' | 'completed' | 'failed'

export interface AgentSessionLink {
  childSessionId: string
  agentName: string
  task: string
  status: AgentSessionStatus
}

export interface ConversationCursor {
  value: string
}

export interface ConversationControlState {
  phase: Phase
  canSubmitPrompt: boolean
  canRequestCompact: boolean
  compactPending: boolean
  compacting: boolean
  currentModeId?: string
  activeTurnId?: string
}

export type ConversationBlock =
  | { kind: 'user'; id: string; text: string }
  | {
      kind: 'assistant'
      id: string
      text: string
      reasoningContent?: string
      status: BlockStatus
    }
  | {
      kind: 'toolCall'
      id: string
      name: string
      arguments: string
      text: string
      status: BlockStatus
      taskId?: string
      metadata?: Record<string, unknown>
    }
  | { kind: 'error'; id: string; message: string }
  | { kind: 'systemNote'; id: string; text: string }
  | {
      kind: 'compactSummary'
      id: string
      summary: string
      trigger: string
      preTokens: number
      postTokens: number
      transcriptPath?: string
    }

export interface ConversationSnapshot {
  sessionId: string
  sessionTitle: string
  cursor: ConversationCursor
  phase: Phase
  control: ConversationControlState
  blocks: ConversationBlock[]
  agentSessions: AgentSessionLink[]
}

// ── SSE Stream ──

export interface ConversationStreamEnvelope {
  sessionId: string
  cursor: ConversationCursor
  delta: ConversationDelta
}

export type ConversationDelta =
  | { kind: 'appendBlock'; block: ConversationBlock }
  | { kind: 'patchBlock'; blockId: string; textDelta: string }
  | { kind: 'finalizeBlock'; block: ConversationBlock }
  | { kind: 'updateControlState'; control: ConversationControlState }
  | { kind: 'rehydrateRequired' }
  | {
      kind: 'sessionContinued'
      parentSessionId: string
      newSessionId: string
      parentCursor: ConversationCursor
    }
  | {
      kind: 'toolOutput'
      callId: string
      stream: ToolOutputStream
      delta: string
    }
  | { kind: 'thinkingDelta'; blockId: string; delta: string }
  | { kind: 'patchArguments'; blockId: string; arguments: string }
  | { kind: 'toolCallBackgrounded'; callId: string; taskId: string }
  | { kind: 'agentSessionUpdated'; agentSession: AgentSessionLink }

// ── App State ──

export interface ConnectionState {
  status: 'disconnected' | 'connecting' | 'connected' | 'error'
  error?: string
}

// ── Config / Models ──

export interface ProfileView {
  name: string
  providerKind: string
  baseUrl: string
  hasApiKey: boolean
  models: ModelView[]
}

export interface ModelView {
  id: string
  maxTokens?: number
  contextLimit?: number
}

export interface ConfigView {
  configPath: string
  activeProfile: string
  activeModel: string
  profiles: ProfileView[]
  warning?: string
}

export interface CurrentModelInfo {
  profileName: string
  modelId: string
  providerKind: string
}

export interface AvailableModel {
  profileName: string
  modelId: string
  providerKind: string
}

export interface ModelTestResult {
  success: boolean
  message: string
}
