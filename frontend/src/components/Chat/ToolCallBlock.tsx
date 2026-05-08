import { memo, useState, useEffect } from 'react'
import type { ConversationBlock } from '../../services/types'
import {
  pillNeutral,
  pillSuccess,
  pillDanger,
  chevronIcon,
  codeBlockShell,
  codeBlockContent,
} from '../../lib/styles'
import { cn } from '../../lib/utils'

interface ToolCallBlockProps {
  block: Extract<ConversationBlock, { kind: 'toolCall' }>
}

function statusPill(status: string): string {
  switch (status) {
    case 'complete':
      return pillSuccess
    case 'error':
      return pillDanger
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
    default:
      return '运行中'
  }
}

function ToolCallBlock({ block }: ToolCallBlockProps) {
  const defaultOpen = block.status === 'error'
  const [isOpen, setIsOpen] = useState(defaultOpen)

  useEffect(() => {
    if (block.status === 'error') setIsOpen(true)
  }, [block.status])

  const displayText =
    block.text || (block.status === 'streaming' ? '等待输出...' : '')

  return (
    <details
      className="group mb-2 ml-[var(--chat-assistant-content-offset)] block min-w-0 max-w-full animate-block-enter motion-reduce:animate-none"
      open={isOpen}
      onToggle={(e) => setIsOpen(e.currentTarget.open)}
    >
      <summary className="flex min-w-0 cursor-pointer items-center gap-2 py-1.5 font-mono text-[13px] leading-relaxed text-text-secondary list-none [&::-webkit-details-marker]:hidden hover:opacity-85">
        <span className={cn('shrink-0', statusPill(block.status))}>
          {block.name}
        </span>
        <span className="min-w-0 flex-1 truncate text-text-primary">
          {displayText.slice(0, 100)}
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
          <div className={codeBlockShell}>
            <pre className={codeBlockContent}>
              <code>{displayText}</code>
            </pre>
          </div>
        </div>
      </div>
    </details>
  )
}

export default memo(ToolCallBlock)
