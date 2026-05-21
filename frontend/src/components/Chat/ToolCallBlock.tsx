import { memo, useState } from 'react'
import type { ConversationBlock } from '../../services/types'
import { extractRenderSpec } from '../../types/render-spec'
import {
  pillNeutral,
  pillSuccess,
  pillDanger,
  chevronIcon,
  codeBlockShell,
  codeBlockContent,
} from '../../lib/styles'
import { cn } from '../../lib/utils'
import { RenderSpecViewer } from './RenderSpecViewer'

interface ToolCallBlockProps {
  block: Extract<ConversationBlock, { kind: 'toolCall' }>
}

function statusPill(status: string): string {
  switch (status) {
    case 'complete':
      return pillSuccess
    case 'error':
      return pillDanger
    case 'backgrounded':
      return pillNeutral
    default:
      return pillNeutral
  }
}

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

function ToolCallBlock({ block }: ToolCallBlockProps) {
  const [isOpen, setIsOpen] = useState(false)

  const renderSpec = extractRenderSpec(
    block.metadata as Record<string, unknown> | undefined
  )

  // 折叠摘要行：显示 LLM 调用的参数（如果有的话），否则回退到结果摘要
  const summaryLine = compactLine(
    block.arguments ||
      block.text ||
      (block.status === 'streaming' ? '等待输出...' : '(无输出)')
  )
  // 展开区域：显示工具执行结果
  const resultText =
    block.text || (block.status === 'streaming' ? '等待输出...' : '')

  return (
    <details
      className="group mb-2 ml-[var(--chat-assistant-content-offset)] block min-w-0 max-w-full animate-block-enter motion-reduce:animate-none"
      open={block.status === 'error' || isOpen}
      onToggle={(e) => setIsOpen(e.currentTarget.open)}
    >
      <summary className="flex min-w-0 cursor-pointer items-center gap-2 py-1.5 font-mono text-[13px] leading-relaxed text-text-secondary list-none [&::-webkit-details-marker]:hidden hover:opacity-85">
        <span className={cn('shrink-0', statusPill(block.status))}>
          {block.name}
        </span>
        <span
          className="block min-w-0 flex-1 overflow-hidden text-ellipsis whitespace-nowrap text-text-primary"
          title={summaryLine}
        >
          {summaryLine}
        </span>
        <span className="shrink-0 text-text-muted">
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
      <div className="mt-2 flex min-w-0 flex-col gap-3 rounded-[18px] border border-border bg-surface-soft px-4 py-3.5 shadow-soft">
        <div className="min-w-0 overflow-y-auto overscroll-contain pr-1 max-h-[min(58vh,560px)]">
          {renderSpec ? (
            <RenderSpecViewer spec={renderSpec} />
          ) : (
            <div className={codeBlockShell}>
              <pre className={codeBlockContent}>
                <code>{resultText}</code>
              </pre>
            </div>
          )}
        </div>
      </div>
    </details>
  )
}

export default memo(ToolCallBlock)
