// Types aligned with astrcode-protocol/src/http.rs DTOs

export type Phase =
  | 'idle'
  | 'thinking'
  | 'streaming'
  | 'calling_tool'
  | 'compacting'
  | 'error'
export type ToolOutputStream = 'stdout' | 'stderr'
export type BlockStatus = 'streaming' | 'complete' | 'error'

// ── Request/Response ──

export interface CreateSessionRequest {
  workingDir: string
}

export interface CreateSessionResponse {
  sessionId: string
}

export interface PromptAttachmentWire {
  filename: string
  content: string
  mediaType: string
}

export interface PromptAttachment {
  id: string
  file: File
  filename: string
  mediaType: string
  previewUrl: string
}

export interface PromptRequest {
  text: string
  attachments?: PromptAttachmentWire[]
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
  requiresIdle: boolean
  argumentCompletions: boolean
  priority: number
  source: 'builtin' | 'plugin' | 'skill' | string
}

export type CommandInvokeResponse =
  | {
      kind: 'display'
      sessionId: string
      content: string
      isError: boolean
    }
  | {
      kind: 'handled'
      sessionId: string
      message: string
    }
  | {
      kind: 'started'
      sessionId: string
      turnId: string
    }

export interface CommandCompletionItem {
  label: string
  insertText: string
  detail?: string
}

export interface CommandCompletionResponse {
  items: CommandCompletionItem[]
  truncated: boolean
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
  shadowedCommands: ShadowedSlashCommandInfo[]
  keybindings: KeybindingInfo[]
  statusItems: StatusItemInfo[]
}

export interface ShadowedSlashCommandInfo {
  name: string
  activeSource: string
  activePriority: number
  shadowedSource: string
  shadowedPriority: number
  shadowedExtensionId: string
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
  sourceExtension?: string
}

export interface SessionListResponse {
  sessions: SessionListItem[]
}

// ── Conversation Snapshot ──

export type AgentSessionStatus = 'running' | 'completed' | 'failed'

export interface AgentSessionLink {
  childSessionId: string
  toolCallId?: string
  agentName?: string
  task?: string
  /** 省略时表示仅更新 phase/currentTool，不改动终态 status */
  status?: AgentSessionStatus
  finalSessionId?: string
  summary?: string
  error?: string
  phase?: Phase
  currentTool?: string
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
  | {
      kind: 'user'
      id: string
      text: string
      attachments?: PromptAttachmentWire[]
      source?: string
    }
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
      argumentsJson?: Record<string, unknown>
      text: string
      status: BlockStatus
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
  | {
      kind: 'patchArguments'
      blockId: string
      arguments: string
      argumentsJson?: Record<string, unknown>
    }
  | { kind: 'agentSessionUpdated'; agentSession: AgentSessionLink }
  | { kind: 'agentSessionRemoved'; childSessionId: string }
  | { kind: 'statusItemUpdate'; id: string; text: string }
  | { kind: 'extensionRegistryChanged' }
  | {
      kind: 'patchToolMetadata'
      blockId: string
      metadata: Record<string, unknown>
    }
  | {
      kind: 'patchToolCall'
      blockId: string
      text: string
      metadata?: Record<string, unknown>
    }

// ── App State ──

export interface ConnectionState {
  status: 'disconnected' | 'connecting' | 'connected' | 'error'
  error?: string
}

// ── Config / Models ──

export interface ProfileView {
  name: string
  providerKind: string
  wireFormat: ProviderWireFormat
  authScheme: ProviderAuthScheme
  baseUrl: string
  hasApiKey: boolean
  models: ModelView[]
}

export type ProviderWireFormat =
  | 'openai_chat_completions'
  | 'openai_responses'
  | 'anthropic_messages'
  | 'google_genai'

export type ProviderAuthScheme =
  | 'none'
  | 'bearer'
  | 'x_api_key'
  | 'x_goog_api_key'

export interface ModelView {
  id: string
  maxTokens?: number
  contextLimit?: number
}

export interface ConfigView {
  configPath: string
  activeProfile: string
  activeModel: string
  activeSmallProfile?: string
  activeSmallModel?: string
  approvalMode: 'manual' | 'yolo'
  extensionStates: Record<string, boolean>
  profiles: ProfileView[]
  warning?: string
}

export interface ProviderCatalogView {
  providers: ProviderSpecView[]
}

export interface ProviderSpecView {
  id: string
  displayName: string
  providerKind: string
  wireFormat: ProviderWireFormat
  authScheme: ProviderAuthScheme
  defaultModel: string
  apiKeyEnvVars: string[]
  endpoints: ProviderEndpointPresetView[]
  capabilities: ProviderSpecCapabilitiesView
}

export interface ProviderEndpointPresetView {
  id: string
  label: string
  baseUrl?: string
  isDefault: boolean
}

export interface ProviderSpecCapabilitiesView {
  promptCacheKey: boolean
  streamUsage: boolean
  reasoningEffort: boolean
}

export interface ApplyProviderPresetRequest {
  providerId: string
  endpointId?: string
  profileName?: string
  baseUrl?: string
  apiKey?: string
  modelId?: string
  activate?: boolean
}

export interface ApplyProviderPresetResponse {
  success: boolean
  profileName: string
  modelId: string
  activated: boolean
  warning?: string
}

export interface RemoveProviderPresetRequest {
  profileName: string
}

export interface RemoveProviderPresetResponse {
  success: boolean
  removedProfileName: string
  activeProfile: string
  activeModel: string
  warning?: string
}

export interface ExtensionStateView {
  extensionId: string
  enabled: boolean
  loaded: boolean
  source: 'builtin' | 'disk' | 'unknown'
  declaration?: ExtensionDeclarationView
  diagnostics?: ExtensionDiagnosticsView
}

export interface ExtensionDeclarationView {
  id: string
  capabilities: string[]
  tools: Record<string, unknown>[]
  dynamicTools: boolean
  commands: Record<string, unknown>[]
  dynamicCommands: boolean
  keybindings: Record<string, unknown>[]
  statusItems: Record<string, unknown>[]
  events: Record<string, unknown>[]
  httpRoutes: ExtensionHttpRouteView[]
}

export interface ExtensionHttpRouteView {
  method: string
  path: string
  description: string
  maxBodyBytes: number
}

export interface ExtensionDiagnosticsView {
  load: ExtensionStageDiagnosticsView
  register: ExtensionStageDiagnosticsView
  start: ExtensionStageDiagnosticsView
  hookCalls: number
  hookTimeouts: number
  lastHook?: string
  lastDurationMs?: number
  lastError?: string
}

export type ExtensionStageStatusView =
  | 'unknown'
  | 'running'
  | 'succeeded'
  | 'failed'
  | 'skipped'

export interface ExtensionStageDiagnosticsView {
  status: ExtensionStageStatusView
  durationMs?: number
  error?: string
}

export interface CurrentModelInfo {
  profileName: string
  modelId: string
  providerKind: string
  wireFormat: ProviderWireFormat
}

export interface AvailableModel {
  profileName: string
  modelId: string
  providerKind: string
  wireFormat: ProviderWireFormat
}

export interface ModelTestResult {
  success: boolean
  message: string
}
