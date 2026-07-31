import { memo } from 'react'
import type { ConversationBlock } from '../../services/types'
import { MarkdownContent } from './MarkdownContent'
import { pillNeutral } from '../../lib/styles'

interface RecapBlockProps {
  block: Extract<ConversationBlock, { kind: 'recap' }>
}

function sourceLabel(source: string | undefined): string {
  return source === 'manual' ? '手动回顾' : source ? `回顾 · ${source}` : '回顾'
}

function RecapBlock({ block }: RecapBlockProps) {
  return (
    <div className="rounded-[18px] border border-border bg-surface-soft px-5 py-4 shadow-soft">
      <div className="flex items-center gap-2 text-[13px]">
        <svg
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          className="shrink-0 text-accent"
        >
          <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8l-6-6Z" />
          <path d="M14 2v6h6" />
          <path d="M9 13h6" />
          <path d="M9 17h6" />
        </svg>
        <span className="font-medium text-text-primary">会话回顾</span>
        <span className={pillNeutral}>{sourceLabel(block.source)}</span>
      </div>

      <div className="mt-3 min-w-0 text-[13.5px] leading-relaxed text-text-secondary">
        <MarkdownContent text={block.text} />
      </div>
    </div>
  )
}

export default memo(RecapBlock)
