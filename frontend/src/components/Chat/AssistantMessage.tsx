import React, { memo } from 'react'
import type { ConversationBlock } from '../../services/types'
import { assistantAvatar } from '../../lib/styles'
import { MarkdownContent, StreamingMarkdown } from './MarkdownContent'
import {
  cachedThinkingExtraction,
  extractThinkingBlocks,
} from './thinkingExtraction'

interface AssistantMessageProps {
  block: Extract<ConversationBlock, { kind: 'assistant' }>
  reasoningText?: string | null
}

function AssistantMessage({ block, reasoningText }: AssistantMessageProps) {
  const streaming = block.status === 'streaming'
  const streamingParts =
    streaming && !reasoningText
      ? cachedThinkingExtraction(block.id, block.text)
      : null
  const staticParts = React.useMemo(() => {
    if (reasoningText || streaming) return null
    return extractThinkingBlocks(block.text)
  }, [block.text, reasoningText, streaming])
  const assistantParts = reasoningText
    ? { visibleText: block.text, thinkingBlocks: [reasoningText] }
    : (streamingParts ?? staticParts ?? { visibleText: '', thinkingBlocks: [] })

  return (
    <div className="flex items-start gap-[16px] animate-message-enter max-sm:gap-[12px] motion-reduce:animate-none">
      <div className={assistantAvatar} aria-hidden="true">
        <svg viewBox="0 0 20 20" className="w-4 h-4">
          <rect
            x="3.25"
            y="3.25"
            width="13.5"
            height="13.5"
            rx="3.5"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.4"
          />
          <path
            d="M8 8h4M8 10h4M8 12h2.5"
            fill="none"
            stroke="currentColor"
            strokeLinecap="round"
            strokeWidth="1.4"
          />
        </svg>
      </div>
      <div className="min-w-0 flex-1 pt-0.5">
        <div className="relative min-w-0 max-w-full overflow-wrap-anywhere bg-transparent py-2 text-text-primary prose-chat">
          {assistantParts.thinkingBlocks.map((thinkingBlock, index) => (
            <details
              key={`thinking-${index}`}
              className="mb-3.5 bg-transparent border-none rounded-0 overflow-visible group"
              open={streaming}
            >
              <summary className="inline-flex items-center gap-2 py-1 min-h-[24px] cursor-pointer select-none bg-transparent border-none rounded-0 text-text-secondary/80 transition-opacity duration-150 ease-out text-[13px] font-medium list-none [&::-webkit-details-marker]:hidden hover:opacity-100">
                <span className="w-4 h-4 inline-flex items-center justify-center shrink-0 text-[13px] text-text-secondary/70">
                  <svg
                    width="15"
                    height="15"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                  >
                    <path d="M12 5a3 3 0 1 0-5.997.125 4 4 0 0 0-2.526 5.77 4 4 0 0 0 .556 6.588A4 4 0 1 0 12 18Z" />
                    <path d="M12 5a3 3 0 1 1 5.997.125 4 4 0 0 1 2.526 5.77 4 4 0 0 1-.556 6.588A4 4 0 1 1 12 18Z" />
                    <path d="M15 13a4.5 4.5 0 0 1-3-4 4.5 4.5 0 0 1-3 4" />
                  </svg>
                </span>
                <span className="font-outfit font-semibold tracking-wide text-text-secondary/75">
                  Thinking
                </span>
                <span className="inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center text-text-secondary opacity-50 transition-transform duration-150 ease-out group-open:rotate-90">
                  <svg
                    width="13"
                    height="13"
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
              <div className="mb-3 ml-2 mt-2 border-l-2 border-border pl-4 overflow-wrap-anywhere text-[13.5px] leading-relaxed text-text-secondary/80 prose-chat">
                {streaming ? (
                  <StreamingMarkdown
                    text={thinkingBlock}
                    cacheKey={`${block.id}:thinking:${index}`}
                  />
                ) : (
                  <MarkdownContent text={thinkingBlock} />
                )}
              </div>
            </details>
          ))}
          {streaming ? (
            assistantParts.visibleText ? (
              <StreamingMarkdown
                text={assistantParts.visibleText}
                cacheKey={`${block.id}:visible`}
              />
            ) : null
          ) : assistantParts.visibleText ? (
            <MarkdownContent text={assistantParts.visibleText} />
          ) : null}
        </div>
      </div>
    </div>
  )
}

export default memo(AssistantMessage)
