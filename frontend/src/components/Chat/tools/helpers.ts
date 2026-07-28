import type { ConversationBlock, ToolCallStatus } from '../../../services/types'

export type ToolCall = Extract<ConversationBlock, { kind: 'toolCall' }>
export type JsonRecord = Record<string, unknown>

export function statusLabel(status: ToolCallStatus): string {
  switch (status) {
    case 'complete':
      return '完成'
    case 'error':
      return '结果错误'
    case 'failed':
      return '执行失败'
    case 'cancelled':
      return '已取消'
    case 'streaming':
      return '运行中'
  }
}

export function compactLine(text: string): string {
  return text.replace(/\s+/g, ' ').trim()
}

export function compactPreviewLine(text: string, maxLength = 160): string {
  const scanLength = Math.max(maxLength, maxLength * 4)
  const compacted = compactLine(text.slice(0, scanLength))
  return compacted.length > maxLength
    ? `${compacted.slice(0, Math.max(0, maxLength - 1))}…`
    : compacted
}

export function asRecord(value: unknown): JsonRecord {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? (value as JsonRecord)
    : {}
}

export function stringValue(source: JsonRecord, ...keys: string[]): string {
  for (const key of keys) {
    const value = source[key]
    if (typeof value === 'string') return value
  }
  return ''
}

export function numberValue(
  source: JsonRecord,
  ...keys: string[]
): number | undefined {
  for (const key of keys) {
    const value = source[key]
    if (typeof value === 'number' && Number.isFinite(value)) return value
  }
  return undefined
}

export function boolValue(
  source: JsonRecord,
  ...keys: string[]
): boolean | undefined {
  for (const key of keys) {
    const value = source[key]
    if (typeof value === 'boolean') return value
  }
  return undefined
}

export function arrayValue(source: JsonRecord, ...keys: string[]): unknown[] {
  for (const key of keys) {
    const value = source[key]
    if (Array.isArray(value)) return value
  }
  return []
}

export function formatBytes(bytes?: number): string {
  if (bytes == null) return ''
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

export function byteChangeLabel(oldBytes?: number, newBytes?: number): string {
  if (oldBytes != null && newBytes != null) {
    return `${formatBytes(oldBytes)} -> ${formatBytes(newBytes)}`
  }
  return formatBytes(newBytes)
}

export function truncateMiddle(text: string, max = 96): string {
  if (text.length <= max) return text
  const head = Math.ceil((max - 1) * 0.58)
  const tail = Math.floor((max - 1) * 0.42)
  return `${text.slice(0, head)}…${text.slice(text.length - tail)}`
}

export interface TextPreview {
  content: string
  truncated: boolean
  omittedCharacters: number
}

export function previewText(
  text: string,
  maxCharacters = 6000,
  maxLines = 24
): TextPreview {
  let end = Math.min(text.length, maxCharacters)
  let lines = 1
  for (let index = 0; index < end; index += 1) {
    if (text[index] !== '\n') continue
    lines += 1
    if (lines > maxLines) {
      end = index
      break
    }
  }

  const truncated = end < text.length
  return {
    content: truncated ? text.slice(0, end) : text,
    truncated,
    omittedCharacters: truncated ? text.length - end : 0,
  }
}

export function countLines(text: string): number {
  if (!text) return 0
  return text.split(/\r\n|\r|\n/).length
}

export function toolArgs(block: ToolCall): JsonRecord {
  return asRecord(block.argumentsJson)
}

export function toolMeta(block: ToolCall): JsonRecord {
  return asRecord(block.metadata)
}

export function pathFor(block: ToolCall): string {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  return stringValue(meta, 'path') || stringValue(args, 'path')
}

export function changesLabel(meta: JsonRecord): string {
  const insertions = numberValue(meta, 'insertions')
  const deletions = numberValue(meta, 'deletions')
  if (insertions != null || deletions != null) {
    return `+${insertions ?? 0} -${deletions ?? 0}`
  }
  const oldBytes = numberValue(meta, 'oldBytes')
  const newBytes = numberValue(meta, 'newBytes')
  if (newBytes != null) {
    return oldBytes != null
      ? `${formatBytes(oldBytes)} -> ${formatBytes(newBytes)}`
      : formatBytes(newBytes)
  }
  return ''
}

export function paginationLabel(meta: JsonRecord): string {
  const hasMore = boolValue(meta, 'hasMore', 'truncated')
  const nextOffset = numberValue(meta, 'nextOffset')
  const nextCharOffset = numberValue(meta, 'nextCharOffset')
  if (!hasMore) return ''
  if (nextOffset != null) return `more at offset ${nextOffset}`
  if (nextCharOffset != null) return `more at char ${nextCharOffset}`
  return 'has more'
}

export function pathScopeLabel(args: JsonRecord, meta: JsonRecord): string {
  return (
    stringValue(meta, 'path', 'root') || stringValue(args, 'path', 'root') || ''
  )
}

export function stringField(
  obj: Record<string, unknown>,
  camel: string,
  snake?: string
): string {
  const v = obj[camel] ?? (snake ? obj[snake] : undefined)
  return typeof v === 'string' ? v : ''
}

export function boolField(
  obj: Record<string, unknown>,
  camel: string
): boolean | undefined {
  const v = obj[camel]
  return typeof v === 'boolean' ? v : undefined
}
