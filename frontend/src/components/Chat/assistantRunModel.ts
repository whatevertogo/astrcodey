import type { ConversationBlock } from '../../services/types'
import { readGateApproval } from '../../tool-ui/components/gateApprovalMeta'
import { readToolUi, readToolUiPhase } from '../../tool-ui/wire'
import { extractThinkingBlocks } from './thinkingExtraction'
import {
  boolValue,
  compactLine,
  numberValue,
  pathFor,
  stringValue,
  toolArgs,
  toolMeta,
  truncateMiddle,
} from './tools/helpers'

export type AssistantLikeBlock = Extract<
  ConversationBlock,
  { kind: 'assistant' | 'toolCall' }
>
export type ToolBlock = Extract<ConversationBlock, { kind: 'toolCall' }>
export type AssistantBlock = Extract<ConversationBlock, { kind: 'assistant' }>

export type MessageListItem =
  | {
      type: 'assistantRun'
      id: string
      blocks: AssistantLikeBlock[]
      index: number
    }
  | {
      type: 'block'
      id: string
      block: Exclude<ConversationBlock, AssistantLikeBlock>
      index: number
    }

export type ActivityKind =
  | 'created'
  | 'edited'
  | 'read'
  | 'command'
  | 'searched'
  | 'tool'

export interface ToolActivity {
  kind: ActivityKind
  title: string
  label: string
  detail?: string
  insertions?: number
  deletions?: number
  block: ToolBlock
}

export interface ThinkingEntry {
  key: string
  blockId: string
  text: string
  streaming: boolean
}

export type ProcessEntry =
  | {
      type: 'thinking'
      id: string
      entry: ThinkingEntry
    }
  | {
      type: 'tool'
      id: string
      activity: ToolActivity
    }

export type AssistantRunSegment =
  | {
      type: 'process'
      id: string
      entries: ProcessEntry[]
      durationSeconds: number
      hasAttention: boolean
      hasStreamingWork: boolean
    }
  | {
      type: 'content'
      id: string
      block: AssistantBlock
    }

export interface AssistantRunModel {
  segments: AssistantRunSegment[]
  processEntries: ProcessEntry[]
  finalReplyBlock: AssistantBlock | null
  status: 'streaming' | 'complete' | 'error'
  durationSeconds: number
  hasAttention: boolean
  hasStreamingWork: boolean
}

function isAssistantLike(
  block: ConversationBlock
): block is AssistantLikeBlock {
  return block.kind === 'assistant' || block.kind === 'toolCall'
}

export function buildMessageListItems(
  blocks: ConversationBlock[]
): MessageListItem[] {
  const items: MessageListItem[] = []
  let index = 0

  while (index < blocks.length) {
    const block = blocks[index]
    if (isAssistantLike(block)) {
      const runBlocks: AssistantLikeBlock[] = []
      const firstIndex = index
      while (index < blocks.length && isAssistantLike(blocks[index])) {
        runBlocks.push(blocks[index] as AssistantLikeBlock)
        index += 1
      }
      items.push({
        type: 'assistantRun',
        id: runBlocks.map((item) => item.id).join(':'),
        blocks: runBlocks,
        index: firstIndex,
      })
      continue
    }

    items.push({
      type: 'block',
      id: block.id,
      block: block as Exclude<ConversationBlock, AssistantLikeBlock>,
      index,
    })
    index += 1
  }

  return items
}

export function streamingMessageListItemId(
  items: MessageListItem[]
): string | null {
  const last = items[items.length - 1]
  if (!last) return null
  if (last.type !== 'assistantRun') return null
  return last.blocks.some((block) => block.status === 'streaming')
    ? last.id
    : null
}

export function assistantThinkingBlocks(block: AssistantBlock): string[] {
  if (block.reasoningContent) return [block.reasoningContent]
  if (block.status === 'streaming') return []
  return extractThinkingBlocks(block.text).thinkingBlocks
}

export function assistantVisibleText(block: AssistantBlock): string {
  if (block.reasoningContent) return block.text
  return extractThinkingBlocks(block.text).visibleText
}

function filenameFor(path: string): string {
  const normalized = path.replace(/\\/g, '/')
  const filename = normalized.split('/').filter(Boolean).pop()
  return truncateMiddle(filename || normalized || '文件', 72)
}

function durationLabel(meta: Record<string, unknown>): string {
  const durationMs = numberValue(meta, 'durationMs', 'duration_ms')
  if (durationMs != null)
    return `${Math.max(1, Math.round(durationMs / 1000))}s`
  const durationSeconds = numberValue(meta, 'duration', 'durationSeconds')
  if (durationSeconds != null)
    return `${Math.max(1, Math.round(durationSeconds))}s`
  return ''
}

function commandLabel(block: ToolBlock): string {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  return compactLine(
    stringValue(meta, 'command') ||
      stringValue(args, 'command') ||
      block.arguments ||
      block.name
  )
}

export function toolActivityFor(block: ToolBlock): ToolActivity {
  const meta = toolMeta(block)
  const path = pathFor(block)
  const insertions = numberValue(meta, 'insertions')
  const deletions = numberValue(meta, 'deletions')

  if (block.name === 'write') {
    const created = boolValue(meta, 'created')
    return {
      kind: created === true ? 'created' : 'edited',
      title: created === true ? '创建文件' : '编辑文件',
      label: filenameFor(path),
      insertions,
      deletions,
      block,
    }
  }

  if (block.name === 'edit' || block.name === 'patch') {
    return {
      kind: 'edited',
      title: block.name === 'patch' ? '应用补丁' : '编辑文件',
      label: filenameFor(path || 'patch'),
      insertions,
      deletions,
      block,
    }
  }

  if (block.name === 'read') {
    return {
      kind: 'read',
      title: '读取文件',
      label: filenameFor(path),
      block,
    }
  }

  if (block.name === 'shell' || block.name === 'terminal') {
    return {
      kind: 'command',
      title: '运行命令',
      label: commandLabel(block),
      detail: durationLabel(meta),
      block,
    }
  }

  if (block.name === 'grep' || block.name === 'find') {
    const args = toolArgs(block)
    const pattern = stringValue(meta, 'pattern') || stringValue(args, 'pattern')
    return {
      kind: 'searched',
      title: '搜索',
      label: truncateMiddle(pattern || path || block.name, 72),
      block,
    }
  }

  return {
    kind: 'tool',
    title: '工具调用',
    label: block.name || '工具',
    block,
  }
}

export function durationSecondsFor(activity: ToolActivity): number {
  const meta = toolMeta(activity.block)
  const durationMs = numberValue(meta, 'durationMs', 'duration_ms')
  if (durationMs != null) return durationMs / 1000
  return numberValue(meta, 'duration', 'durationSeconds') ?? 0
}

export function formatRunDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return ''
  if (seconds < 60) return `${Math.max(1, Math.round(seconds))}s`
  const minutes = Math.floor(seconds / 60)
  const restSeconds = Math.round(seconds % 60)
  return restSeconds > 0 ? `${minutes}m ${restSeconds}s` : `${minutes}m`
}

export function processSummaryTitle({
  hasStreamingWork,
  durationSeconds,
}: {
  hasStreamingWork: boolean
  durationSeconds: number
}): string {
  if (hasStreamingWork) return '处理中'
  const durationLabel = formatRunDuration(durationSeconds)
  return durationLabel ? `已处理 ${durationLabel}` : '已处理'
}

export function thinkingEntriesFor(block: AssistantBlock): ThinkingEntry[] {
  return assistantThinkingBlocks(block).map((text, index) => ({
    key: `${block.id}:thinking:${index}`,
    blockId: block.id,
    text,
    streaming: block.status === 'streaming',
  }))
}

export function finalReplyBlockFor(
  blocks: AssistantLikeBlock[]
): AssistantBlock | null {
  for (let index = blocks.length - 1; index >= 0; index -= 1) {
    const block = blocks[index]
    if (block.kind !== 'assistant') continue
    if (assistantVisibleText(block).trim()) return block
  }
  return null
}

function toolHasAttention(block: ToolBlock): boolean {
  if (block.status === 'error') return true
  if (readGateApproval(block.metadata)?.pending === true) return true

  const meta = toolMeta(block)
  const toolUi = readToolUi(meta)
  if (toolUi?.approval && readToolUiPhase(meta) !== 'result') return true

  return false
}

export function buildAssistantRunModel(
  blocks: AssistantLikeBlock[]
): AssistantRunModel {
  const finalReplyBlock = finalReplyBlockFor(blocks)
  const segments = buildRunSegments(blocks)
  const processEntries = segments.flatMap((segment) =>
    segment.type === 'process' ? segment.entries : []
  )
  const status = blocks.some((block) => block.status === 'error')
    ? 'error'
    : blocks.some((block) => block.status === 'streaming')
      ? 'streaming'
      : 'complete'
  const hasStreamingWork = processEntries.some(
    (entry) =>
      (entry.type === 'thinking' && entry.entry.streaming) ||
      (entry.type === 'tool' && entry.activity.block.status === 'streaming')
  )
  const durationSeconds = processEntries.reduce(
    (sum, entry) =>
      entry.type === 'tool' ? sum + durationSecondsFor(entry.activity) : sum,
    0
  )
  const hasAttention = processEntries.some(
    (entry) => entry.type === 'tool' && toolHasAttention(entry.activity.block)
  )

  return {
    segments,
    processEntries,
    finalReplyBlock,
    status,
    durationSeconds,
    hasAttention,
    hasStreamingWork,
  }
}

function processSegmentFor(entries: ProcessEntry[]): AssistantRunSegment {
  const durationSeconds = entries.reduce(
    (sum, entry) =>
      entry.type === 'tool' ? sum + durationSecondsFor(entry.activity) : sum,
    0
  )
  const hasAttention = entries.some(
    (entry) => entry.type === 'tool' && toolHasAttention(entry.activity.block)
  )
  const hasStreamingWork = entries.some(
    (entry) =>
      (entry.type === 'thinking' && entry.entry.streaming) ||
      (entry.type === 'tool' && entry.activity.block.status === 'streaming')
  )

  return {
    type: 'process',
    id: entries.map((entry) => entry.id).join(':'),
    entries,
    durationSeconds,
    hasAttention,
    hasStreamingWork,
  }
}

function buildRunSegments(blocks: AssistantLikeBlock[]): AssistantRunSegment[] {
  const segments: AssistantRunSegment[] = []
  let pendingProcessEntries: ProcessEntry[] = []

  const flushProcess = () => {
    if (pendingProcessEntries.length === 0) return
    segments.push(processSegmentFor(pendingProcessEntries))
    pendingProcessEntries = []
  }

  for (const block of blocks) {
    if (block.kind === 'toolCall') {
      pendingProcessEntries.push({
        type: 'tool',
        id: block.id,
        activity: toolActivityFor(block),
      })
      continue
    }

    pendingProcessEntries.push(
      ...thinkingEntriesFor(block).map((entry) => ({
        type: 'thinking' as const,
        id: entry.key,
        entry,
      }))
    )

    if (!assistantVisibleText(block).trim()) continue
    flushProcess()
    segments.push({
      type: 'content',
      id: `${block.id}:content`,
      block,
    })
  }

  flushProcess()
  return segments
}
