import { memo, useState } from 'react'
import type { ReactNode } from 'react'
import type { ConversationBlock } from '../../services/types'
import { extractRenderSpec } from '../../types/render-spec'
import { chevronIcon } from '../../lib/styles'
import { cn } from '../../lib/utils'
import { RenderSpecViewer } from './RenderSpecViewer'

interface ToolCallBlockProps {
  block: Extract<ConversationBlock, { kind: 'toolCall' }>
}

type ToolCall = Extract<ConversationBlock, { kind: 'toolCall' }>
type JsonRecord = Record<string, unknown>

function statusLabel(status: string): string {
  switch (status) {
    case 'complete':
      return '完成'
    case 'error':
      return '失败'
    case 'backgrounded':
      return '后台运行中'
    default:
      return '运行中'
  }
}

function compactLine(text: string): string {
  return text.replace(/\s+/g, ' ').trim()
}

function asRecord(value: unknown): JsonRecord {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? (value as JsonRecord)
    : {}
}

function stringValue(source: JsonRecord, ...keys: string[]): string {
  for (const key of keys) {
    const value = source[key]
    if (typeof value === 'string') return value
  }
  return ''
}

function numberValue(
  source: JsonRecord,
  ...keys: string[]
): number | undefined {
  for (const key of keys) {
    const value = source[key]
    if (typeof value === 'number' && Number.isFinite(value)) return value
  }
  return undefined
}

function boolValue(source: JsonRecord, ...keys: string[]): boolean | undefined {
  for (const key of keys) {
    const value = source[key]
    if (typeof value === 'boolean') return value
  }
  return undefined
}

function formatBytes(bytes?: number): string {
  if (bytes == null) return ''
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

function byteChangeLabel(oldBytes?: number, newBytes?: number): string {
  if (oldBytes != null && newBytes != null) {
    return `${formatBytes(oldBytes)} -> ${formatBytes(newBytes)}`
  }
  return formatBytes(newBytes)
}

function truncateMiddle(text: string, max = 96): string {
  if (text.length <= max) return text
  const head = Math.ceil((max - 1) * 0.58)
  const tail = Math.floor((max - 1) * 0.42)
  return `${text.slice(0, head)}…${text.slice(text.length - tail)}`
}

function previewText(text: string, max = 12000): string {
  if (text.length <= max) return text
  return `${text.slice(0, max)}\n\n… truncated ${text.length - max} characters`
}

function countLines(text: string): number {
  if (!text) return 0
  return text.split(/\r\n|\r|\n/).length
}

function toolArgs(block: ToolCall): JsonRecord {
  return asRecord(block.argumentsJson)
}

function toolMeta(block: ToolCall): JsonRecord {
  return asRecord(block.metadata)
}

function pathFor(block: ToolCall): string {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  return stringValue(meta, 'path') || stringValue(args, 'path')
}

function changesLabel(meta: JsonRecord): string {
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

function summaryForTool(block: ToolCall): string {
  const args = toolArgs(block)
  const meta = toolMeta(block)

  if (block.name === 'shell') {
    const command = stringValue(meta, 'command') || stringValue(args, 'command')
    return command ? `$ ${compactLine(command)}` : ''
  }

  if (block.name === 'write') {
    const path = pathFor(block)
    const created = boolValue(meta, 'created')
    const action =
      created === true ? 'create' : created === false ? 'write' : 'write'
    return compactLine(
      [action, path && truncateMiddle(path), changesLabel(meta)]
        .filter(Boolean)
        .join(' ')
    )
  }

  if (block.name === 'edit') {
    const path = pathFor(block)
    const replacements = numberValue(meta, 'replacements')
    const operations = numberValue(meta, 'operationCount')
    const count =
      replacements != null
        ? `${replacements} replacement${replacements === 1 ? '' : 's'}`
        : operations != null
          ? `${operations} edit${operations === 1 ? '' : 's'}`
          : ''
    return compactLine(
      ['edit', path && truncateMiddle(path), count, changesLabel(meta)]
        .filter(Boolean)
        .join(' ')
    )
  }

  return ''
}

/**
 * 从 agent 工具的原始 JSON 参数构造 streaming 阶段的 RenderSpec。
 * 直接读结构化字段，不依赖后端格式化字符串。
 */
function stringField(
  obj: Record<string, unknown>,
  camel: string,
  snake?: string
): string {
  const v = obj[camel] ?? (snake ? obj[snake] : undefined)
  return typeof v === 'string' ? v : ''
}

function boolField(
  obj: Record<string, unknown>,
  camel: string
): boolean | undefined {
  const v = obj[camel]
  return typeof v === 'boolean' ? v : undefined
}

function buildStreamingAgentSpec(
  argsJson: Record<string, unknown>
): import('../../types/render-spec').RenderSpec {
  const entries: { key: string; value: string; tone: 'accent' | 'muted' }[] = []

  const description = stringField(argsJson, 'description')
  const agent = stringField(argsJson, 'subagentType', 'subagent_type')
  const model = stringField(argsJson, 'model')
  const rawMode = boolField(argsJson, 'waitForResult')
  const mode = rawMode !== undefined ? (rawMode ? 'sync' : 'async') : ''

  if (description)
    entries.push({ key: 'task', value: description, tone: 'accent' })
  if (agent) entries.push({ key: 'agent', value: agent, tone: 'accent' })
  if (model) entries.push({ key: 'model', value: model, tone: 'muted' })
  if (mode) entries.push({ key: 'mode', value: mode, tone: 'muted' })

  const prompt = stringField(argsJson, 'prompt')

  return {
    type: 'box',
    children: [
      ...(entries.length > 0
        ? [
            {
              type: 'key_value' as const,
              entries,
              tone: undefined as undefined,
            },
          ]
        : []),
      ...(prompt
        ? [
            {
              type: 'text' as const,
              text: `prompt: ${prompt.slice(0, 180)}`,
              tone: 'muted' as const,
            },
          ]
        : []),
    ],
  }
}

function StatusIndicatorDot({ status }: { status: string }) {
  const dotColor =
    status === 'complete'
      ? 'bg-success'
      : status === 'error'
        ? 'bg-danger'
        : 'bg-accent-strong animate-pulse'
  return <span className={cn('h-1.5 w-1.5 rounded-full shrink-0', dotColor)} />
}

function MetaRow({ label, value }: { label: string; value?: string | number }) {
  if (value === undefined || value === '') return null
  return (
    <div className="flex min-w-0 items-baseline gap-2">
      <dt className="shrink-0 text-text-muted">{label}</dt>
      <dd className="min-w-0 break-words text-code-text">{value}</dd>
    </div>
  )
}

function MetaGrid({ children }: { children: ReactNode }) {
  return (
    <dl className="grid min-w-0 grid-cols-1 gap-x-5 gap-y-1.5 font-mono text-[12px] leading-relaxed sm:grid-cols-2">
      {children}
    </dl>
  )
}

function CodePreview({
  text,
  tone = 'default',
}: {
  text: string
  tone?: 'default' | 'diff' | 'stderr'
}) {
  const content = previewText(text)
  const color =
    tone === 'stderr'
      ? 'text-danger'
      : tone === 'diff'
        ? 'text-code-text'
        : 'text-code-text'
  const children =
    tone === 'diff' ? <DiffCodeLines text={content} /> : <code>{content}</code>

  return (
    <pre
      className={cn(
        'm-0 max-h-[min(52vh,520px)] overflow-auto whitespace-pre-wrap break-words pt-3 font-mono text-[12.5px] leading-relaxed',
        color
      )}
      children={children}
    />
  )
}

function DiffCodeLines({ text }: { text: string }) {
  return (
    <code className="block min-w-0">
      {text.split('\n').map((line, index) => {
        const isFileHeader = line.startsWith('+++') || line.startsWith('---')
        const isAddition = line.startsWith('+') && !isFileHeader
        const isDeletion = line.startsWith('-') && !isFileHeader
        const isHunk = line.startsWith('@@')

        return (
          <span
            key={index}
            className={cn(
              'block min-w-fit -mx-4 px-4',
              isAddition && 'bg-success-soft/70 text-success',
              isDeletion && 'bg-danger-soft/70 text-danger',
              isFileHeader && 'text-text-muted',
              isHunk && 'bg-surface text-text-secondary'
            )}
          >
            {line || ' '}
          </span>
        )
      })}
    </code>
  )
}

function FileToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const path = pathFor(block)
  const diff = stringValue(meta, 'diff')
  const content = stringValue(args, 'content')
  const oldStr = stringValue(args, 'oldStr', 'old_string')
  const newStr = stringValue(args, 'newStr', 'new_string')
  const edits = Array.isArray(args.edits) ? args.edits : []
  const created = boolValue(meta, 'created')
  const oldBytes = numberValue(meta, 'oldBytes')
  const newBytes = numberValue(meta, 'newBytes')
  const operationCount = numberValue(meta, 'operationCount')
  const replacements = numberValue(meta, 'replacements')
  const insertions = numberValue(meta, 'insertions')
  const deletions = numberValue(meta, 'deletions')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <MetaGrid>
          <MetaRow label="path" value={path} />
          <MetaRow
            label="action"
            value={
              block.name === 'write'
                ? created
                  ? 'created'
                  : 'overwritten'
                : 'edited'
            }
          />
          <MetaRow label="size" value={changesLabel(meta)} />
          <MetaRow
            label="ops"
            value={
              block.name === 'edit'
                ? [
                    operationCount != null && `${operationCount} op`,
                    replacements != null && `${replacements} repl`,
                  ]
                    .filter(Boolean)
                    .join(' / ')
                : undefined
            }
          />
          <MetaRow
            label="lines"
            value={
              content
                ? countLines(content)
                : insertions != null || deletions != null
                  ? `+${insertions ?? 0} / -${deletions ?? 0}`
                  : undefined
            }
          />
          <MetaRow
            label="bytes"
            value={
              oldBytes != null || newBytes != null
                ? byteChangeLabel(oldBytes, newBytes)
                : undefined
            }
          />
        </MetaGrid>
      </div>

      {diff ? (
        <CodePreview text={diff} tone="diff" />
      ) : content ? (
        <CodePreview text={content} />
      ) : oldStr || newStr ? (
        <div className="space-y-3 pt-3">
          {oldStr && (
            <div>
              <div className="mb-1 font-mono text-[11px] font-semibold uppercase tracking-wide text-text-muted">
                old
              </div>
              <CodePreview text={oldStr} />
            </div>
          )}
          {newStr && (
            <div>
              <div className="mb-1 font-mono text-[11px] font-semibold uppercase tracking-wide text-text-muted">
                new
              </div>
              <CodePreview text={newStr} />
            </div>
          )}
        </div>
      ) : edits.length > 0 ? (
        <div className="space-y-2 pt-3 font-mono text-[12px] text-code-text">
          {edits.slice(0, 8).map((edit, index) => {
            const item = asRecord(edit)
            const itemOld = stringValue(item, 'oldStr', 'old_string')
            const itemNew = stringValue(item, 'newStr', 'new_string')
            return (
              <div key={index} className="min-w-0">
                <span className="text-text-muted">#{index + 1}</span>{' '}
                <span>{truncateMiddle(compactLine(itemOld), 80)}</span>
                <span className="text-text-muted"> -&gt; </span>
                <span>{truncateMiddle(compactLine(itemNew), 80)}</span>
              </div>
            )
          })}
          {edits.length > 8 && (
            <div className="text-text-muted">
              +{edits.length - 8} more edits
            </div>
          )}
        </div>
      ) : (
        <CodePreview text={block.text || 'No file preview available.'} />
      )}
    </div>
  )
}

function ShellToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const command = stringValue(meta, 'command') || stringValue(args, 'command')
  const intent = stringValue(meta, 'intent') || stringValue(args, 'intent')
  const cwd = stringValue(meta, 'cwd') || stringValue(args, 'cwd')
  const shell = stringValue(meta, 'shell')
  const exitCode = numberValue(meta, 'exitCode')
  const timeout = numberValue(args, 'timeout')
  const timedOut = boolValue(meta, 'timedOut')
  const stdoutBytes = numberValue(meta, 'stdoutBytes')
  const stderrBytes = numberValue(meta, 'stderrBytes')
  const stdin = stringValue(args, 'stdin')
  const runInBackground = boolValue(args, 'runInBackground')
  const output =
    block.text || (block.status === 'streaming' ? 'Waiting for output...' : '')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <div className="mb-3 font-mono text-[13px] leading-relaxed text-code-text">
          <span className="select-none text-text-muted">$ </span>
          <span className="break-words">{command || '(empty command)'}</span>
        </div>
        <MetaGrid>
          <MetaRow label="cwd" value={cwd} />
          <MetaRow label="shell" value={shell} />
          <MetaRow
            label="exit"
            value={
              timedOut
                ? 'timed out'
                : exitCode != null
                  ? String(exitCode)
                  : undefined
            }
          />
          <MetaRow
            label="timeout"
            value={timeout != null ? `${timeout}s` : undefined}
          />
          <MetaRow
            label="output"
            value={[
              formatBytes(stdoutBytes),
              formatBytes(stderrBytes) && `stderr ${formatBytes(stderrBytes)}`,
            ]
              .filter(Boolean)
              .join(' / ')}
          />
          <MetaRow
            label="mode"
            value={runInBackground ? 'background' : undefined}
          />
          <MetaRow label="intent" value={intent} />
          <MetaRow
            label="stdin"
            value={stdin ? `${formatBytes(stdin.length)} piped` : undefined}
          />
        </MetaGrid>
      </div>
      <CodePreview
        text={output || '(no output)'}
        tone={block.status === 'error' ? 'stderr' : 'default'}
      />
    </div>
  )
}

function DefaultToolDetails({ block }: { block: ToolCall }) {
  const resultText =
    block.text || (block.status === 'streaming' ? '等待输出...' : '')

  return <CodePreview text={resultText} />
}

function ToolDetails({
  block,
  renderSpec,
  agentSpec,
}: {
  block: ToolCall
  renderSpec?: import('../../types/render-spec').RenderSpec
  agentSpec?: import('../../types/render-spec').RenderSpec
}) {
  if (renderSpec) return <RenderSpecViewer spec={renderSpec} />
  if (agentSpec) return <RenderSpecViewer spec={agentSpec} />
  if (block.name === 'write' || block.name === 'edit') {
    return <FileToolDetails block={block} />
  }
  if (block.name === 'shell') {
    return <ShellToolDetails block={block} />
  }
  return <DefaultToolDetails block={block} />
}

function ToolCallBlock({ block }: ToolCallBlockProps) {
  const [isOpen, setIsOpen] = useState(false)

  const renderSpec = extractRenderSpec(
    block.metadata as Record<string, unknown> | undefined
  )

  const summaryLine = compactLine(
    summaryForTool(block) ||
      block.arguments ||
      block.text ||
      (block.status === 'streaming' ? '等待输出...' : '(无输出)')
  )
  const agentSpec =
    block.name === 'agent' && block.argumentsJson && !renderSpec
      ? buildStreamingAgentSpec(block.argumentsJson)
      : undefined

  return (
    <details
      className="group mb-1 ml-[var(--chat-assistant-content-offset)] block min-w-0 max-w-full animate-block-enter motion-reduce:animate-none"
      open={block.status === 'error' || isOpen || !!agentSpec}
      onToggle={(e) => setIsOpen(e.currentTarget.open)}
    >
      <summary className="flex min-w-0 cursor-pointer items-center gap-3 py-2 font-mono text-[13px] leading-relaxed text-text-secondary list-none [&::-webkit-details-marker]:hidden hover:opacity-90 select-none">
        <span className="inline-flex items-center gap-1.5 px-2 py-0.5 rounded-md bg-surface border border-border font-mono text-[11px] font-semibold text-text-secondary uppercase tracking-wider shrink-0">
          <StatusIndicatorDot status={block.status} />
          {block.name}
        </span>
        <span
          className="block min-w-0 flex-1 overflow-hidden text-ellipsis whitespace-nowrap text-text-secondary/85 text-[12.5px] font-mono opacity-90"
          title={summaryLine}
        >
          {summaryLine}
        </span>
        <span className="shrink-0 text-[11px] font-semibold uppercase tracking-wider text-text-muted">
          {statusLabel(block.status)}
        </span>
        <span className={chevronIcon}>
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <polyline points="9 18 15 12 9 6"></polyline>
          </svg>
        </span>
      </summary>
      <div className="mt-1.5 flex min-w-0 flex-col rounded-xl border border-border bg-code-surface px-4 py-3 shadow-soft">
        <div className="min-w-0 overflow-y-auto overscroll-contain pr-1 max-h-[min(58vh,560px)]">
          <ToolDetails
            block={block}
            renderSpec={renderSpec}
            agentSpec={agentSpec}
          />
        </div>
      </div>
    </details>
  )
}

export default memo(ToolCallBlock)
