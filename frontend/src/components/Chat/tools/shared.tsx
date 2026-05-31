import type { ReactNode } from 'react'
import type { RenderSpec } from '../../../types/render-spec'
import { cn } from '../../../lib/utils'
import { toolCodePreviewBleed } from '../../../lib/styles'
import { DiffCodeLines } from '../DiffCodeLines'
import { previewText, type ToolCall } from './helpers'

export function StatusIndicatorDot({
  status,
  pendingApproval,
}: {
  status: string
  pendingApproval?: boolean
}) {
  const dotColor = pendingApproval
    ? 'bg-warning animate-pulse'
    : status === 'complete'
      ? 'bg-success'
      : status === 'error'
        ? 'bg-danger'
        : 'bg-accent-strong animate-pulse'
  return <span className={cn('h-1.5 w-1.5 shrink-0 rounded-full', dotColor)} />
}

export function MetaRow({
  label,
  value,
}: {
  label: string
  value?: string | number
}) {
  if (value === undefined || value === '') return null
  return (
    <div className="flex min-w-0 items-baseline gap-2">
      <dt className="shrink-0 text-text-muted">{label}</dt>
      <dd className="min-w-0 wrap-break-word text-code-text">{value}</dd>
    </div>
  )
}

export function MetaGrid({ children }: { children: ReactNode }) {
  return (
    <dl className="grid min-w-0 grid-cols-1 gap-x-5 gap-y-1.5 font-mono text-[12px] leading-relaxed sm:grid-cols-2">
      {children}
    </dl>
  )
}

export function CodePreview({
  text,
  tone = 'default',
}: {
  text: string
  tone?: 'default' | 'diff' | 'stderr'
}) {
  const content = previewText(text)
  const color = tone === 'stderr' ? 'text-danger' : 'text-code-text'
  const children =
    tone === 'diff' ? (
      <DiffCodeLines text={content} />
    ) : (
      <code>{content}</code>
    )

  return (
    <pre
      className={cn(
        toolCodePreviewBleed,
        'm-0 min-w-fit whitespace-pre pt-3 font-mono text-[12.5px] leading-relaxed',
        color
      )}
    >
      {children}
    </pre>
  )
}

function lineNumberColumnWidth(numbers: string[]): string {
  const maxLen = numbers.reduce((max, value) => Math.max(max, value.length), 1)
  return `${maxLen}ch`
}

export function ReadContentPreview({ text }: { text: string }) {
  const lines = previewText(text).split('\n')
  const parsed = lines.map((line) => {
    const match = line.match(/^\s*(\d+)\t(.*)$/)
    return match ? { number: match[1], code: match[2] } : undefined
  })
  const hasLineNumbers = parsed.some(Boolean)

  if (!hasLineNumbers) {
    return <CodePreview text={text} />
  }

  const lineNumbers = parsed.flatMap((item) => (item ? [item.number] : []))
  const lineNumWidth = lineNumberColumnWidth(lineNumbers)

  return (
    <div
      className={cn(
        toolCodePreviewBleed,
        'min-w-fit pt-3 font-mono text-[12.5px] leading-relaxed text-code-text'
      )}
    >
      {lines.map((line, index) => {
        const item = parsed[index]
        return (
          <div
            key={index}
            className="grid min-w-fit gap-x-2"
            style={{
              gridTemplateColumns: `${lineNumWidth} minmax(0,max-content)`,
            }}
          >
            <span className="select-none text-right tabular-nums text-text-muted">
              {item?.number ?? ''}
            </span>
            <code className="whitespace-pre">{(item?.code ?? line) || ' '}</code>
          </div>
        )
      })}
    </div>
  )
}

export function buildStreamingAgentSpec(
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

export function DefaultToolDetails({ block }: { block: ToolCall }) {
  const resultText =
    block.text || (block.status === 'streaming' ? '等待输出...' : '')
  return <CodePreview text={resultText} />
}
