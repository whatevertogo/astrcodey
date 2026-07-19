import {
  AGENT_SESSION_STATUSES,
  BLOCK_STATUSES,
  PHASES,
  TOOL_OUTPUT_STREAMS,
} from './types'
import type {
  AgentSessionLink,
  ConversationBlock,
  ConversationControlState,
  ConversationCursor,
  ConversationDelta,
  ConversationSnapshot,
  ConversationStreamEnvelope,
  PromptAttachmentWire,
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

/** 缺省或 `null` 视为 `[]`（与 serde `skip_serializing_if` 省略字段对齐）。 */
function optionalArrayField(source: JsonObject, name: string): unknown[] {
  const value = source[name]
  if (value == null) return []
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

function stringEnumDecoder<const Values extends readonly string[]>(
  context: string,
  values: Values,
  fallback?: Values[number]
): (value: unknown) => Values[number] {
  const members = new Set<string>(values)
  return (value) => {
    if (typeof value === 'string' && members.has(value)) {
      return value as Values[number]
    }
    if (fallback !== undefined) return fallback
    throw new ProtocolDecodeError(`invalid ${context} ${String(value)}`)
  }
}

const decodePhase = stringEnumDecoder('phase', PHASES)
const decodeBlockStatus = stringEnumDecoder('block status', BLOCK_STATUSES)
const decodeToolOutputStream = stringEnumDecoder(
  'tool output stream',
  TOOL_OUTPUT_STREAMS
)
const decodeAgentSessionStatus = stringEnumDecoder(
  'agent session status',
  AGENT_SESSION_STATUSES,
  'running'
)
export function decodeConversationCursor(value: unknown): ConversationCursor {
  const object = decodeObject(value, 'cursor')
  return { value: requiredString(object, 'value') }
}

function decodePromptAttachmentWire(value: unknown): PromptAttachmentWire {
  const object = decodeObject(value, 'prompt attachment')
  return {
    filename: requiredString(object, 'filename'),
    content: requiredString(object, 'content'),
    mediaType: requiredString(object, 'mediaType'),
  }
}

export function decodeConversationBlock(value: unknown): ConversationBlock {
  const object = decodeObject(value, 'conversation block')
  const kind = requiredString(object, 'kind')
  const id = requiredString(object, 'id')

  switch (kind) {
    case 'user': {
      const rawAttachments = optionalArrayField(object, 'attachments')
      const attachments =
        rawAttachments.length > 0
          ? rawAttachments.map(decodePromptAttachmentWire)
          : undefined
      return {
        kind,
        id,
        text: requiredString(object, 'text'),
        attachments,
        source: optionalString(object, 'source'),
      }
    }
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
        metadata: optionalObject(object, 'metadata'),
        argumentsJson:
          object.argumentsJson && typeof object.argumentsJson === 'object'
            ? (object.argumentsJson as Record<string, unknown>)
            : undefined,
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
        argumentsJson:
          object.argumentsJson && typeof object.argumentsJson === 'object'
            ? (object.argumentsJson as Record<string, unknown>)
            : undefined,
      }
    case 'agentSessionUpdated':
      return {
        kind,
        agentSession: decodeAgentSessionLink(object.agentSession),
      }
    case 'agentSessionRemoved':
      return {
        kind,
        childSessionId: requiredString(object, 'childSessionId'),
      }
    case 'statusItemUpdate':
      return {
        kind,
        id: requiredString(object, 'id'),
        text: requiredString(object, 'text'),
      }
    case 'extensionRegistryChanged':
      return { kind }
    case 'patchToolMetadata':
      return {
        kind,
        blockId: requiredString(object, 'blockId'),
        metadata: optionalObject(object, 'metadata') ?? {},
      }
    case 'patchToolCall':
      return {
        kind,
        blockId: requiredString(object, 'blockId'),
        text: requiredString(object, 'text'),
        metadata: optionalObject(object, 'metadata'),
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

function decodeAgentSessionLink(value: unknown): AgentSessionLink {
  const object = decodeObject(value, 'agent session link')
  return {
    childSessionId: requiredString(object, 'childSessionId'),
    toolCallId: optionalString(object, 'toolCallId'),
    agentName: optionalString(object, 'agentName'),
    task: optionalString(object, 'task'),
    status:
      object.status == null
        ? undefined
        : decodeAgentSessionStatus(object.status),
    finalSessionId: optionalString(object, 'finalSessionId'),
    summary: optionalString(object, 'summary'),
    error: optionalString(object, 'error'),
    phase: object.phase == null ? undefined : decodePhase(object.phase),
    currentTool: optionalString(object, 'currentTool'),
  }
}
