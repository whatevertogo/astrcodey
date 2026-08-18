import type {
  AgentSessionLink,
  AgentSessionUpdate,
  ConversationBlock,
  SlashCommandInfo,
} from '../../services/types'
import { parseSlashCommand } from '../../lib/keybindings'

export function mergeBlock(
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
    const metadata = incoming.metadata ?? current.metadata
    const argumentsJson = incoming.argumentsJson ?? current.argumentsJson
    return {
      ...incoming,
      name: incoming.name.trim() ? incoming.name : current.name,
      arguments: incoming.arguments.trim()
        ? incoming.arguments
        : current.arguments,
      text: incoming.text.trim() ? incoming.text : current.text,
      ...(metadata ? { metadata } : {}),
      ...(argumentsJson ? { argumentsJson } : {}),
    }
  }

  return incoming
}

export function upsertBlock(
  blocks: ConversationBlock[],
  block: ConversationBlock
): ConversationBlock[] {
  const idx = blocks.findIndex((item) => item.id === block.id)
  if (idx === -1) return [...blocks, block]

  const next = [...blocks]
  next[idx] = mergeBlock(next[idx], block)
  return next
}

export function applyAgentSessionUpdate(
  current: AgentSessionLink | undefined,
  update: AgentSessionUpdate
): AgentSessionLink | undefined {
  switch (update.kind) {
    case 'spawned':
      return {
        childSessionId: update.childSessionId,
        toolCallId: update.toolCallId,
        agentName: update.agentName,
        task: update.task,
        status: 'running',
        phase: 'thinking',
      }
    case 'completed':
      return current
        ? {
            ...current,
            status: 'completed',
            finalSessionId: update.finalSessionId,
            summary: update.summary,
            error: undefined,
            phase: undefined,
            currentTool: undefined,
          }
        : undefined
    case 'failed':
      return current
        ? {
            ...current,
            status: 'failed',
            finalSessionId: update.finalSessionId,
            summary: undefined,
            error: update.error,
            phase: undefined,
            currentTool: undefined,
          }
        : undefined
    case 'progress':
      return current?.status === 'running'
        ? {
            ...current,
            phase: update.phase,
            currentTool: update.currentTool,
          }
        : current
  }
}

export function commandNoteBlock(message: string): ConversationBlock {
  return {
    kind: 'systemNote',
    id: `command-${Date.now()}`,
    text: message,
  }
}

/**
 * 是否为宿主执行的 compact 命令。
 *
 * 依据 server 下发的命令元数据判断（`execution.kind === 'host'` +
 * `command === 'compact_session'`），不再按命令名硬编码：若第三方扩展以更高
 * priority 遮蔽 `/compact`，此处会正确地把它当普通扩展命令处理。
 */
export function isCompactCommand(
  text: string,
  commands: SlashCommandInfo[]
): boolean {
  const parsed = parseSlashCommand(text)
  if (!parsed || !parsed.name) return false
  const command = commands.find((c) => c.name.toLowerCase() === parsed.name)
  return (
    command?.execution.kind === 'host' &&
    command.execution.command === 'compact_session'
  )
}

export async function withTimeout<T>(
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
