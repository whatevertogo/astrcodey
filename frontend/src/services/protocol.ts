import type {
  AgentSessionLink,
  AgentSessionStatus,
  AvailableModel,
  CompactSessionResponse,
  ConfigView,
  ConversationBlock,
  ConversationControlState,
  ConversationCursor,
  ConversationDelta,
  ConversationSnapshot,
  ConversationStreamEnvelope,
  CreateSessionResponse,
  CurrentModelInfo,
  ModelTestResult,
  ModelView,
  Phase,
  ProfileView,
  PromptSubmitResponse,
  SlashCommandInfo,
  SlashCommandListResponse,
  KeybindingInfo,
  StatusItemInfo,
  SessionListItem,
  SessionListResponse,
  ToolOutputStream,
} from './types'

type JsonObject = Record<string, unknown>

export class ProtocolDecodeError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ProtocolDecodeError'
  }
}

function isObject(value: unknown): value is JsonObject {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function arrayField(source: JsonObject, name: string): unknown[] {
  const value = source[name]
  if (!Array.isArray(value))
    throw new ProtocolDecodeError(`expected array ${name}`)
  return value
}

function requiredString(source: JsonObject, name: string): string {
  const value = source[name]
  if (typeof value !== 'string')
    throw new ProtocolDecodeError(`expected string ${name}`)
  return value
}

function optionalString(source: JsonObject, name: string): string | undefined {
  const value = source[name]
  if (value == null || typeof value !== 'string') return undefined
  return value
}

function requiredBoolean(source: JsonObject, name: string): boolean {
  const value = source[name]
  if (typeof value !== 'boolean') {
    throw new ProtocolDecodeError(`expected boolean ${name}`)
  }
  return value
}

function optionalObject(
  source: JsonObject,
  name: string
): Record<string, unknown> | undefined {
  const value = source[name]
  if (value == null || typeof value !== 'object' || Array.isArray(value))
    return undefined
  return value as Record<string, unknown>
}

function optionalNumber(source: JsonObject, name: string): number | undefined {
  const value = source[name]
  if (value == null) return undefined
  if (typeof value !== 'number') {
    throw new ProtocolDecodeError(`expected number ${name}`)
  }
  return value
}

function decodeObject(value: unknown, context: string): JsonObject {
  if (!isObject(value))
    throw new ProtocolDecodeError(`expected object ${context}`)
  return value
}

function decodePhase(value: unknown): Phase {
  if (
    value === 'idle' ||
    value === 'thinking' ||
    value === 'streaming' ||
    value === 'calling_tool' ||
    value === 'compacting' ||
    value === 'error'
  ) {
    return value
  }
  throw new ProtocolDecodeError(`invalid phase ${String(value)}`)
}

function decodeBlockStatus(
  value: unknown
): 'streaming' | 'complete' | 'error' | 'backgrounded' {
  if (
    value === 'streaming' ||
    value === 'complete' ||
    value === 'error' ||
    value === 'backgrounded'
  ) {
    return value
  }
  throw new ProtocolDecodeError(`invalid block status ${String(value)}`)
}

function decodeToolOutputStream(value: unknown): ToolOutputStream {
  if (value === 'stdout' || value === 'stderr') return value
  throw new ProtocolDecodeError(`invalid tool output stream ${String(value)}`)
}

export function decodeConversationCursor(value: unknown): ConversationCursor {
  const object = decodeObject(value, 'cursor')
  return { value: requiredString(object, 'value') }
}

export function decodeConversationBlock(value: unknown): ConversationBlock {
  const object = decodeObject(value, 'conversation block')
  const kind = requiredString(object, 'kind')
  const id = requiredString(object, 'id')

  switch (kind) {
    case 'user':
      return { kind, id, text: requiredString(object, 'text') }
    case 'assistant':
      return {
        kind,
        id,
        text: requiredString(object, 'text'),
        reasoningContent: optionalString(object, 'reasoningContent'),
        status: decodeBlockStatus(object.status),
      }
    case 'toolCall':
      return {
        kind,
        id,
        name: requiredString(object, 'name'),
        arguments: requiredString(object, 'arguments'),
        text: requiredString(object, 'text'),
        status: decodeBlockStatus(object.status),
        taskId: optionalString(object, 'taskId'),
        metadata: optionalObject(object, 'metadata'),
      }
    case 'error':
      return { kind, id, message: requiredString(object, 'message') }
    case 'systemNote':
      return { kind, id, text: requiredString(object, 'text') }
    case 'compactSummary':
      return {
        kind,
        id,
        summary: requiredString(object, 'summary'),
        trigger: requiredString(object, 'trigger'),
        preTokens: optionalNumber(object, 'preTokens') ?? 0,
        postTokens: optionalNumber(object, 'postTokens') ?? 0,
        transcriptPath: optionalString(object, 'transcriptPath'),
      }
    default:
      throw new ProtocolDecodeError(`invalid block kind ${kind}`)
  }
}

export function decodeConversationControlState(
  value: unknown
): ConversationControlState {
  const object = decodeObject(value, 'control')
  return {
    phase: decodePhase(object.phase),
    canSubmitPrompt: requiredBoolean(object, 'canSubmitPrompt'),
    canRequestCompact: requiredBoolean(object, 'canRequestCompact'),
    compactPending: requiredBoolean(object, 'compactPending'),
    compacting: requiredBoolean(object, 'compacting'),
    currentModeId: optionalString(object, 'currentModeId'),
    activeTurnId: optionalString(object, 'activeTurnId'),
  }
}

export function decodeConversationDelta(value: unknown): ConversationDelta {
  const object = decodeObject(value, 'conversation delta')
  const kind = requiredString(object, 'kind')

  switch (kind) {
    case 'appendBlock':
      return { kind, block: decodeConversationBlock(object.block) }
    case 'patchBlock':
      return {
        kind,
        blockId: requiredString(object, 'blockId'),
        textDelta: requiredString(object, 'textDelta'),
      }
    case 'finalizeBlock':
      return { kind, block: decodeConversationBlock(object.block) }
    case 'updateControlState':
      return { kind, control: decodeConversationControlState(object.control) }
    case 'rehydrateRequired':
      return { kind }
    case 'sessionContinued':
      return {
        kind,
        parentSessionId: requiredString(object, 'parentSessionId'),
        newSessionId: requiredString(object, 'newSessionId'),
        parentCursor: decodeConversationCursor(object.parentCursor),
      }
    case 'toolOutput':
      return {
        kind,
        callId: requiredString(object, 'callId'),
        stream: decodeToolOutputStream(object.stream),
        delta: requiredString(object, 'delta'),
      }
    case 'thinkingDelta':
      return {
        kind,
        blockId: requiredString(object, 'blockId'),
        delta: requiredString(object, 'delta'),
      }
    case 'patchArguments':
      return {
        kind,
        blockId: requiredString(object, 'blockId'),
        arguments: requiredString(object, 'arguments'),
      }
    case 'toolCallBackgrounded':
      return {
        kind,
        callId: requiredString(object, 'callId'),
        taskId: requiredString(object, 'taskId'),
      }
    case 'agentSessionUpdated':
      return {
        kind,
        agentSession: decodeAgentSessionLink(object.agentSession),
      }
    case 'statusItemUpdate':
      return {
        kind,
        id: requiredString(object, 'id'),
        text: requiredString(object, 'text'),
      }
    default:
      throw new ProtocolDecodeError(`invalid delta kind ${kind}`)
  }
}

export function decodeConversationStreamEnvelope(
  value: unknown
): ConversationStreamEnvelope {
  const object = decodeObject(value, 'conversation stream envelope')
  return {
    sessionId: requiredString(object, 'sessionId'),
    cursor: decodeConversationCursor(object.cursor),
    delta: decodeConversationDelta(object.delta),
  }
}

export function tryDecodeConversationStreamEnvelope(
  value: unknown
): ConversationStreamEnvelope | null {
  try {
    return decodeConversationStreamEnvelope(value)
  } catch {
    return null
  }
}

export function decodeCreateSessionResponse(
  value: unknown
): CreateSessionResponse {
  const object = decodeObject(value, 'create session response')
  return { sessionId: requiredString(object, 'sessionId') }
}

export function decodeSessionListResponse(value: unknown): SessionListResponse {
  const object = decodeObject(value, 'session list response')
  return {
    sessions: arrayField(object, 'sessions').map(decodeSessionListItem),
  }
}

function decodeSessionListItem(value: unknown): SessionListItem {
  const object = decodeObject(value, 'session list item')
  return {
    sessionId: requiredString(object, 'sessionId'),
    workingDir: requiredString(object, 'workingDir'),
    displayName: requiredString(object, 'displayName'),
    title: requiredString(object, 'title'),
    createdAt: requiredString(object, 'createdAt'),
    updatedAt: requiredString(object, 'updatedAt'),
    parentSessionId: optionalString(object, 'parentSessionId'),
    parentStorageSeq: optionalNumber(object, 'parentStorageSeq'),
    phase: decodePhase(object.phase),
    firstUserMessage: optionalString(object, 'firstUserMessage'),
  }
}

export function decodeConversationSnapshot(
  value: unknown
): ConversationSnapshot {
  const object = decodeObject(value, 'conversation snapshot')
  return {
    sessionId: requiredString(object, 'sessionId'),
    sessionTitle: requiredString(object, 'sessionTitle'),
    cursor: decodeConversationCursor(object.cursor),
    phase: decodePhase(object.phase),
    control: decodeConversationControlState(object.control),
    blocks: arrayField(object, 'blocks').map(decodeConversationBlock),
    agentSessions: arrayField(object, 'agentSessions').map(
      decodeAgentSessionLink
    ),
  }
}

function decodeAgentSessionStatus(value: unknown): AgentSessionStatus {
  if (value === 'running' || value === 'completed' || value === 'failed')
    return value
  return 'running'
}

function decodeAgentSessionLink(value: unknown): AgentSessionLink {
  const object = decodeObject(value, 'agent session link')
  return {
    childSessionId: requiredString(object, 'childSessionId'),
    agentName: requiredString(object, 'agentName'),
    task: requiredString(object, 'task'),
    status: decodeAgentSessionStatus(object.status),
  }
}

export function decodePromptSubmitResponse(
  value: unknown
): PromptSubmitResponse {
  const object = decodeObject(value, 'prompt submit response')
  const kind = requiredString(object, 'kind')
  switch (kind) {
    case 'accepted':
      return {
        kind,
        sessionId: requiredString(object, 'sessionId'),
        turnId: requiredString(object, 'turnId'),
        branchedFromSessionId: optionalString(object, 'branchedFromSessionId'),
      }
    case 'handled':
      return {
        kind,
        sessionId: requiredString(object, 'sessionId'),
        message: requiredString(object, 'message'),
      }
    default:
      throw new ProtocolDecodeError(`invalid prompt response kind ${kind}`)
  }
}

export function decodeCompactSessionResponse(
  value: unknown
): CompactSessionResponse {
  const object = decodeObject(value, 'compact session response')
  return {
    accepted: requiredBoolean(object, 'accepted'),
    deferred: requiredBoolean(object, 'deferred'),
    newSessionId: optionalString(object, 'newSessionId'),
    message: requiredString(object, 'message'),
  }
}

function decodeSlashCommandInfo(value: unknown): SlashCommandInfo {
  const object = decodeObject(value, 'slash command info')
  return {
    name: requiredString(object, 'name'),
    description: requiredString(object, 'description'),
    needsArgument: requiredBoolean(object, 'needsArgument'),
    source: requiredString(object, 'source'),
  }
}

function decodeKeybindingInfo(value: unknown): KeybindingInfo {
  const object = decodeObject(value, 'keybinding info')
  return {
    key: requiredString(object, 'key'),
    command: requiredString(object, 'command'),
    arguments: optionalString(object, 'arguments') ?? '',
    description: requiredString(object, 'description'),
  }
}

function decodeStatusItemInfo(value: unknown): StatusItemInfo {
  const object = decodeObject(value, 'status item info')
  return {
    id: requiredString(object, 'id'),
    text: requiredString(object, 'text'),
    priority: optionalNumber(object, 'priority') ?? 0,
  }
}

export function decodeSlashCommandListResponse(
  value: unknown
): SlashCommandListResponse {
  const object = decodeObject(value, 'slash command list response')
  return {
    commands: arrayField(object, 'commands').map(decodeSlashCommandInfo),
    keybindings: ((object.keybindings as unknown[]) ?? []).map(
      decodeKeybindingInfo
    ),
    statusItems: ((object.statusItems as unknown[]) ?? []).map(
      decodeStatusItemInfo
    ),
  }
}

export function decodeDeleteProjectResponse(value: unknown): {
  deletedCount: number
} {
  const object = decodeObject(value, 'delete project response')
  const deletedCount = optionalNumber(object, 'deletedCount')
  if (deletedCount == null) {
    throw new ProtocolDecodeError('expected number deletedCount')
  }
  return { deletedCount }
}

export function decodeConfigView(value: unknown): ConfigView {
  const object = decodeObject(value, 'config view')
  const extensionStates: Record<string, boolean> = (() => {
    const raw = object['extensionStates']
    if (raw == null || typeof raw !== 'object' || Array.isArray(raw)) return {}
    return Object.fromEntries(
      Object.entries(raw as Record<string, unknown>)
        .filter(([, v]) => typeof v === 'boolean')
        .map(([k, v]) => [k, v as boolean])
    )
  })()
  return {
    configPath: requiredString(object, 'configPath'),
    activeProfile: requiredString(object, 'activeProfile'),
    activeModel: requiredString(object, 'activeModel'),
    extensionStates,
    profiles: arrayField(object, 'profiles').map(decodeProfileView),
    warning: optionalString(object, 'warning'),
  }
}

function decodeProfileView(value: unknown): ProfileView {
  const object = decodeObject(value, 'profile view')
  return {
    name: requiredString(object, 'name'),
    providerKind: requiredString(object, 'providerKind'),
    baseUrl: requiredString(object, 'baseUrl'),
    hasApiKey: requiredBoolean(object, 'hasApiKey'),
    models: arrayField(object, 'models').map(decodeModelView),
  }
}

function decodeModelView(value: unknown): ModelView {
  const object = decodeObject(value, 'model view')
  return {
    id: requiredString(object, 'id'),
    maxTokens: optionalNumber(object, 'maxTokens'),
    contextLimit: optionalNumber(object, 'contextLimit'),
  }
}

export function decodeConfigReloadResponse(value: unknown): {
  activeProfile: string
  activeModel: string
} {
  const object = decodeObject(value, 'config reload response')
  return {
    activeProfile: requiredString(object, 'activeProfile'),
    activeModel: requiredString(object, 'activeModel'),
  }
}

export function decodeActiveSelectionResponse(value: unknown): {
  success: boolean
  warning?: string
} {
  const object = decodeObject(value, 'active selection response')
  return {
    success: requiredBoolean(object, 'success'),
    warning: optionalString(object, 'warning'),
  }
}

export function decodeCurrentModelInfo(value: unknown): CurrentModelInfo {
  const object = decodeObject(value, 'current model info')
  return {
    profileName: requiredString(object, 'profileName'),
    modelId: requiredString(object, 'modelId'),
    providerKind: requiredString(object, 'providerKind'),
  }
}

export function decodeAvailableModels(value: unknown): AvailableModel[] {
  const object = decodeObject(value, 'model list response')
  return arrayField(object, 'models').map(decodeAvailableModel)
}

function decodeAvailableModel(value: unknown): AvailableModel {
  const object = decodeObject(value, 'available model')
  return {
    profileName: requiredString(object, 'profileName'),
    modelId: requiredString(object, 'modelId'),
    providerKind: requiredString(object, 'providerKind'),
  }
}

export function decodeModelTestResult(value: unknown): ModelTestResult {
  const object = decodeObject(value, 'model test result')
  return {
    success: requiredBoolean(object, 'success'),
    message: requiredString(object, 'message'),
  }
}
