import { useState, type ReactNode } from 'react'
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
  const [expanded, setExpanded] = useState(false)
  const preview = previewText(text)
  const content = expanded ? text : preview.content
  const color = tone === 'stderr' ? 'text-danger' : 'text-code-text'
  const children =
    tone === 'diff' ? <DiffCodeLines text={content} /> : <code>{content}</code>

  return (
    <div className="min-w-0">
      <pre
        className={cn(
          toolCodePreviewBleed,
          'm-0 min-w-fit whitespace-pre pt-3 font-mono text-[12.5px] leading-relaxed',
          color
        )}
      >
        {children}
      </pre>
      {preview.truncated ? (
        <button
          type="button"
          className="mt-2 text-[12px] font-medium text-text-muted transition-colors hover:text-text-primary"
          onClick={() => setExpanded((current) => !current)}
        >
          {expanded
            ? '收起完整输出'
            : `显示完整输出 · 另有 ${preview.omittedCharacters.toLocaleString()} 字符`}
        </button>
      ) : null}
    </div>
  )
}

function lineNumberColumnWidth(numbers: string[]): string {
  const maxLen = numbers.reduce((max, value) => Math.max(max, value.length), 1)
  return `${maxLen}ch`
}

export function ReadContentPreview({ text }: { text: string }) {
  const [expanded, setExpanded] = useState(false)
  const preview = previewText(text)
  const content = expanded ? text : preview.content
  const lines = content.split('\n')
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
    <div className="min-w-0">
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
              <code className="whitespace-pre">
                {(item?.code ?? line) || ' '}
              </code>
            </div>
          )
        })}
      </div>
      {preview.truncated ? (
        <button
          type="button"
          className="mt-2 text-[12px] font-medium text-text-muted transition-colors hover:text-text-primary"
          onClick={() => setExpanded((current) => !current)}
        >
          {expanded
            ? '收起完整输出'
            : `显示完整输出 · 另有 ${preview.omittedCharacters.toLocaleString()} 字符`}
        </button>
      ) : null}
    </div>
  )
}

export function DefaultToolDetails({ block }: { block: ToolCall }) {
  const resultText =
    block.text || (block.status === 'streaming' ? '等待输出...' : '')
  return <CodePreview text={resultText} />
}
