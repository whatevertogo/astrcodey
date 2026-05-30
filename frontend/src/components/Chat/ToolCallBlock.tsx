import { memo, useState } from 'react'
import type { ReactNode } from 'react'
import type { ConversationBlock } from '../../services/types'
import type { RenderSpec } from '../../types/render-spec'
import {
  extractRenderSpec,
  extractRenderSummary,
} from '../../types/render-spec'
import { chevronIcon } from '../../lib/styles'
import { cn } from '../../lib/utils'
import { RenderSpecViewer } from './RenderSpecViewer'
import {
  getToolRenderer,
  registerToolRenderer,
  type ToolRenderer,
  type ToolRendererContext,
} from './toolRendererRegistry'
import { DiffCodeLines } from './DiffCodeLines'

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

function arrayValue(source: JsonRecord, ...keys: string[]): unknown[] {
  for (const key of keys) {
    const value = source[key]
    if (Array.isArray(value)) return value
  }
  return []
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

function paginationLabel(meta: JsonRecord): string {
  const hasMore = boolValue(meta, 'hasMore', 'truncated')
  const nextOffset = numberValue(meta, 'nextOffset')
  const nextCharOffset = numberValue(meta, 'nextCharOffset')
  if (!hasMore) return ''
  if (nextOffset != null) return `more at offset ${nextOffset}`
  if (nextCharOffset != null) return `more at char ${nextCharOffset}`
  return 'has more'
}

function pathScopeLabel(args: JsonRecord, meta: JsonRecord): string {
  return (
    stringValue(meta, 'path', 'root') || stringValue(args, 'path', 'root') || ''
  )
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
): RenderSpec {
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
      <dd className="min-w-0 wrap-break-word text-code-text">{value}</dd>
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
    tone === 'diff' ? (
      <DiffCodeLines text={content} lineClassName="-mx-4 px-4" />
    ) : (
      <code>{content}</code>
    )

  return (
    <pre
      className={cn(
        'm-0 overflow-x-auto whitespace-pre pt-3 font-mono text-[12.5px] leading-relaxed',
        color
      )}
      children={children}
    />
  )
}

function ReadContentPreview({ text }: { text: string }) {
  const lines = previewText(text).split('\n')
  const parsed = lines.map((line) => {
    const match = line.match(/^\s*(\d+)\t(.*)$/)
    return match ? { number: match[1], code: match[2] } : undefined
  })
  const hasLineNumbers = parsed.some(Boolean)

  if (!hasLineNumbers) {
    return <CodePreview text={text} />
  }

  return (
    <div className="overflow-x-auto pt-3 font-mono text-[12.5px] leading-relaxed text-code-text">
      {lines.map((line, index) => {
        const item = parsed[index]
        return (
          <div
            key={index}
            className="grid min-w-fit grid-cols-[4.5rem_minmax(0,1fr)] gap-3"
          >
            <span className="select-none text-right text-text-muted">
              {item?.number ?? ''}
            </span>
            <code className="min-w-0 whitespace-pre">
              {(item?.code ?? line) || ' '}
            </code>
          </div>
        )
      })}
    </div>
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
  const output =
    block.text || (block.status === 'streaming' ? 'Waiting for output...' : '')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <div className="mb-3 font-mono text-[13px] leading-relaxed text-code-text">
          <span className="select-none text-text-muted">$ </span>
          <span className="wrap-break-word">
            {command || '(empty command)'}
          </span>
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

function ReadToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const path = pathFor(block)
  const offset = numberValue(meta, 'offset') ?? numberValue(args, 'offset')
  const shownLines = numberValue(meta, 'shownLines')
  const totalLines = numberValue(meta, 'totalLines')
  const returnedChars = numberValue(meta, 'returnedChars')
  const charOffset =
    numberValue(meta, 'charOffset') ?? numberValue(args, 'charOffset')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <MetaGrid>
          <MetaRow label="path" value={path} />
          <MetaRow
            label="lines"
            value={
              shownLines != null && totalLines != null
                ? `${shownLines}/${totalLines}`
                : shownLines
            }
          />
          <MetaRow label="offset" value={offset} />
          <MetaRow label="chars" value={returnedChars} />
          <MetaRow label="charOffset" value={charOffset} />
          <MetaRow label="next" value={paginationLabel(meta)} />
        </MetaGrid>
      </div>
      <ReadContentPreview text={block.text || '(no content)'} />
    </div>
  )
}

function SearchToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const isFind = block.name === 'find'
  const pattern = stringValue(meta, 'pattern') || stringValue(args, 'pattern')
  const scope = pathScopeLabel(args, meta)
  const outputMode = stringValue(meta, 'outputMode') || 'files_with_matches'
  const returned = numberValue(meta, 'returned') ?? numberValue(meta, 'count')
  const totalMatches = numberValue(meta, 'totalMatches')
  const skippedFiles = numberValue(meta, 'skippedFiles')
  const glob = stringValue(args, 'glob')
  const fileType = stringValue(args, 'fileType')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <MetaGrid>
          <MetaRow label="pattern" value={pattern} />
          <MetaRow label="scope" value={scope} />
          <MetaRow label="mode" value={isFind ? 'files' : outputMode} />
          <MetaRow
            label="returned"
            value={
              totalMatches != null && isFind
                ? `${returned ?? 0}/${totalMatches}`
                : returned
            }
          />
          <MetaRow label="glob" value={glob} />
          <MetaRow label="type" value={fileType} />
          <MetaRow label="skipped" value={skippedFiles} />
          <MetaRow label="next" value={paginationLabel(meta)} />
        </MetaGrid>
      </div>
      <CodePreview text={block.text || '(no results)'} />
    </div>
  )
}

function PatchToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const patch = stringValue(args, 'patch')
  const files = arrayValue(meta, 'files')
  const applied = numberValue(meta, 'filesApplied', 'filesChanged')
  const failed = numberValue(meta, 'filesFailed')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <MetaGrid>
          <MetaRow label="applied" value={applied} />
          <MetaRow label="failed" value={failed} />
          <MetaRow label="files" value={files.length || undefined} />
        </MetaGrid>
      </div>
      {files.length > 0 && (
        <div className="space-y-1 py-3 font-mono text-[12px] leading-relaxed">
          {files.slice(0, 12).map((file, index) => {
            const item = asRecord(file)
            const applied = boolValue(item, 'applied')
            const changeType = stringValue(item, 'changeType') || 'changed'
            const path = stringValue(item, 'path')
            const error = stringValue(item, 'error')
            return (
              <div key={index} className="flex min-w-0 items-baseline gap-2">
                <span
                  className={cn(
                    'shrink-0 text-[11px] font-semibold uppercase',
                    applied === false ? 'text-danger' : 'text-success'
                  )}
                >
                  {applied === false ? 'failed' : changeType}
                </span>
                <span className="min-w-0 flex-1 wrap-break-word text-code-text">
                  {path || '(unknown path)'}
                </span>
                {error && (
                  <span className="min-w-0 wrap-break-word text-danger">
                    {error}
                  </span>
                )}
              </div>
            )
          })}
          {files.length > 12 && (
            <div className="text-text-muted">
              +{files.length - 12} more files
            </div>
          )}
        </div>
      )}
      {patch ? (
        <CodePreview text={patch} tone="diff" />
      ) : (
        <CodePreview text={block.text || '(no patch preview)'} />
      )}
    </div>
  )
}

function TerminalToolDetails({ block }: { block: ToolCall }) {
  const args = toolArgs(block)
  const meta = toolMeta(block)
  const action = stringValue(args, 'action')
  const id = stringValue(meta, 'id') || stringValue(args, 'id')
  const command = stringValue(meta, 'command') || stringValue(args, 'command')
  const cwd = stringValue(args, 'cwd')
  const input = stringValue(args, 'input')
  const waitMs = numberValue(args, 'waitMs')
  const bytesSent = numberValue(meta, 'bytesSent')
  const droppedBytes = numberValue(meta, 'droppedBytes')
  const exitCode = numberValue(meta, 'exitCode')
  const alive = boolValue(meta, 'alive')
  const count = numberValue(meta, 'count')
  const terminals = arrayValue(meta, 'terminals')

  return (
    <div className="min-w-0 divide-y divide-border/70">
      <div className="pb-3">
        <MetaGrid>
          <MetaRow label="action" value={action} />
          <MetaRow label="id" value={id} />
          <MetaRow label="command" value={command} />
          <MetaRow label="cwd" value={cwd} />
          <MetaRow
            label="alive"
            value={alive != null ? (alive ? 'yes' : 'no') : undefined}
          />
          <MetaRow label="exit" value={exitCode} />
          <MetaRow label="sent" value={formatBytes(bytesSent)} />
          <MetaRow label="dropped" value={formatBytes(droppedBytes)} />
          <MetaRow
            label="wait"
            value={waitMs != null ? `${waitMs}ms` : undefined}
          />
          <MetaRow
            label="input"
            value={input ? `${formatBytes(input.length)} piped` : undefined}
          />
          <MetaRow label="count" value={count} />
        </MetaGrid>
      </div>
      {terminals.length > 0 && (
        <div className="space-y-1 py-3 font-mono text-[12px] text-code-text">
          {terminals.slice(0, 10).map((terminal, index) => (
            <div key={index}>{String(terminal)}</div>
          ))}
          {terminals.length > 10 && (
            <div className="text-text-muted">
              +{terminals.length - 10} more terminals
            </div>
          )}
        </div>
      )}
      <CodePreview text={block.text || '(no output)'} />
    </div>
  )
}

function DefaultToolDetails({ block }: { block: ToolCall }) {
  const resultText =
    block.text || (block.status === 'streaming' ? '等待输出...' : '')

  return <CodePreview text={resultText} />
}

function ToolDetails({
  context,
  renderer,
}: {
  context: ToolRendererContext
  renderer?: ToolRenderer
}) {
  if (context.renderSpec) return <RenderSpecViewer spec={context.renderSpec} />
  if (context.agentSpec) return <RenderSpecViewer spec={context.agentSpec} />
  const rendered = renderer?.render?.(context)
  if (rendered != null) return rendered
  return <DefaultToolDetails block={context.block} />
}

const builtinToolRenderers: ToolRenderer[] = [
  {
    id: 'builtin:read',
    priority: 100,
    match: ({ block }) => block.name === 'read',
    summary: ({ block, meta }) => {
      const path = pathFor(block)
      const shownLines = numberValue(meta, 'shownLines')
      const totalLines = numberValue(meta, 'totalLines')
      const linePart =
        shownLines != null && totalLines != null
          ? `${shownLines}/${totalLines} lines`
          : shownLines != null
            ? `${shownLines} lines`
            : ''
      return compactLine(
        ['read', path && truncateMiddle(path), linePart, paginationLabel(meta)]
          .filter(Boolean)
          .join(' ')
      )
    },
    render: ({ block }) => <ReadToolDetails block={block} />,
  },
  {
    id: 'builtin:grep',
    priority: 100,
    match: ({ block }) => block.name === 'grep',
    summary: ({ args, meta }) => {
      const pattern =
        stringValue(meta, 'pattern') || stringValue(args, 'pattern')
      const returned = numberValue(meta, 'returned')
      const outputMode = stringValue(meta, 'outputMode') || 'files_with_matches'
      return compactLine(
        [
          'grep',
          pattern && `"${truncateMiddle(pattern, 60)}"`,
          returned != null && `${returned} result${returned === 1 ? '' : 's'}`,
          outputMode,
          paginationLabel(meta),
        ]
          .filter(Boolean)
          .join(' ')
      )
    },
    render: ({ block }) => <SearchToolDetails block={block} />,
  },
  {
    id: 'builtin:find',
    priority: 100,
    match: ({ block }) => block.name === 'find',
    summary: ({ args, meta }) => {
      const pattern =
        stringValue(meta, 'pattern') || stringValue(args, 'pattern')
      const count = numberValue(meta, 'count')
      const total = numberValue(meta, 'totalMatches')
      const countPart =
        count != null && total != null
          ? `${count}/${total} files`
          : count != null
            ? `${count} files`
            : ''
      return compactLine(
        [
          'find',
          pattern && truncateMiddle(pattern, 64),
          countPart,
          paginationLabel(meta),
        ]
          .filter(Boolean)
          .join(' ')
      )
    },
    render: ({ block }) => <SearchToolDetails block={block} />,
  },
  {
    id: 'builtin:file-change',
    priority: 100,
    match: ({ block }) => block.name === 'write' || block.name === 'edit',
    summary: ({ block, meta }) => {
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
    },
    render: ({ block }) => <FileToolDetails block={block} />,
  },
  {
    id: 'builtin:shell',
    priority: 100,
    match: ({ block }) => block.name === 'shell',
    summary: ({ args, meta }) => {
      const command =
        stringValue(meta, 'command') || stringValue(args, 'command')
      return command ? `$ ${compactLine(command)}` : ''
    },
    render: ({ block }) => <ShellToolDetails block={block} />,
  },
  {
    id: 'builtin:patch',
    priority: 100,
    match: ({ block }) => block.name === 'patch',
    summary: ({ meta }) => {
      const applied = numberValue(meta, 'filesApplied', 'filesChanged') ?? 0
      const failed = numberValue(meta, 'filesFailed') ?? 0
      return compactLine(
        ['patch', `${applied} applied`, failed > 0 && `${failed} failed`]
          .filter(Boolean)
          .join(' ')
      )
    },
    render: ({ block }) => <PatchToolDetails block={block} />,
  },
  {
    id: 'builtin:terminal',
    priority: 100,
    match: ({ block }) => block.name === 'terminal',
    summary: ({ args, meta }) => {
      const action = stringValue(args, 'action')
      const id = stringValue(meta, 'id') || stringValue(args, 'id')
      const command =
        stringValue(meta, 'command') || stringValue(args, 'command')
      const bytesSent = numberValue(meta, 'bytesSent')
      const alive = boolValue(meta, 'alive')
      const exitCode = numberValue(meta, 'exitCode')
      const count = numberValue(meta, 'count')
      const status =
        alive != null
          ? alive
            ? 'alive'
            : 'exited'
          : exitCode != null
            ? `exit ${exitCode}`
            : ''
      return compactLine(
        [
          'terminal',
          action,
          command && truncateMiddle(command, 72),
          id && truncateMiddle(id, 36),
          bytesSent != null && formatBytes(bytesSent),
          count != null && `${count} active`,
          status,
        ]
          .filter(Boolean)
          .join(' ')
      )
    },
    render: ({ block }) => <TerminalToolDetails block={block} />,
  },
]

builtinToolRenderers.forEach(registerToolRenderer)

function ToolCallBlock({ block }: ToolCallBlockProps) {
  const [isOpen, setIsOpen] = useState(false)
  const args = toolArgs(block)
  const meta = toolMeta(block)

  const renderSpec = extractRenderSpec(block.metadata)
  const agentSpec =
    block.name === 'agent' && block.argumentsJson && !renderSpec
      ? buildStreamingAgentSpec(block.argumentsJson)
      : undefined
  const context: ToolRendererContext = {
    block,
    args,
    meta,
    renderSpec,
    agentSpec,
  }
  const renderer = getToolRenderer(context)

  const summaryLine = compactLine(
    extractRenderSummary(block.metadata) ||
      renderer?.summary?.(context) ||
      block.arguments ||
      block.text ||
      (block.status === 'streaming' ? '等待输出...' : '(无输出)')
  )

  return (
    <details
      className="group mb-1 ml-(--chat-assistant-content-offset) block min-w-0 max-w-full animate-block-enter motion-reduce:animate-none"
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
          <ToolDetails context={context} renderer={renderer} />
        </div>
      </div>
    </details>
  )
}

export default memo(ToolCallBlock)
