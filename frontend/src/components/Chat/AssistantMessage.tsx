import React, { memo, useState, useCallback, Component } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { ConversationBlock } from '../../services/types'
import {
  assistantAvatar,
  codeBlockShell,
  codeBlockHeader,
  codeBlockContent,
  ghostIconButton,
} from '../../lib/styles'
import { cn } from '../../lib/utils'

interface AssistantMessageProps {
  block: Extract<ConversationBlock, { kind: 'assistant' }>
  reasoningText?: string | null
}

class MarkdownGuard extends Component<
  { fallback: string; children: React.ReactNode },
  { hasError: boolean }
> {
  state = { hasError: false }
  static getDerivedStateFromError() {
    return { hasError: true }
  }
  render() {
    if (this.state.hasError) {
      return (
        <pre className="m-0 whitespace-pre-wrap overflow-wrap-anywhere font-inherit text-inherit">
          {this.props.fallback}
        </pre>
      )
    }
    return this.props.children
  }
}

function CopyButton({ code }: { code: string }) {
  const [copied, setCopied] = useState(false)
  const handleCopy = useCallback(() => {
    void navigator.clipboard.writeText(code).then(() => {
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    })
  }, [code])

  return (
    <button
      className={cn(
        ghostIconButton,
        'h-7 gap-1.5 rounded px-2 text-[13px] opacity-0 translate-y-0.5 group-hover:translate-y-0 group-hover:opacity-100'
      )}
      onClick={handleCopy}
      title="复制代码"
    >
      {copied ? (
        <>
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
            <polyline points="20 6 9 17 4 12"></polyline>
          </svg>
          <span>已复制</span>
        </>
      ) : (
        <>
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
            <rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect>
            <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path>
          </svg>
          <span>复制</span>
        </>
      )}
    </button>
  )
}

interface CodeBlockRendererProps extends React.ComponentPropsWithoutRef<'code'> {
  node?: { parent?: { tagName?: string } }
  inline?: boolean
}

function CodeBlockRenderer({
  node,
  className,
  children,
  ...props
}: CodeBlockRendererProps) {
  const match = /language-(\w+)/.exec(className || '')
  const language = match ? match[1] : ''
  const isInline =
    !match &&
    !String(children).includes('\n') &&
    node?.parent?.tagName !== 'pre'

  if (isInline) {
    return (
      <code className={className} {...props}>
        {children}
      </code>
    )
  }

  const codeText = String(children).replace(/^\n/, '').replace(/\n$/, '')
  return (
    <div className={codeBlockShell}>
      <div className={codeBlockHeader}>
        <span className="text-xs lowercase">{language || 'text'}</span>
        <CopyButton code={codeText} />
      </div>
      <pre className={codeBlockContent} {...props}>
        <code className={className}>{codeText}</code>
      </pre>
    </div>
  )
}

const markdownComponents = {
  pre: ({ children }: React.PropsWithChildren) => <>{children}</>,
  code: CodeBlockRenderer as React.ComponentType<
    React.ComponentPropsWithoutRef<'code'>
  >,
}

const MarkdownContent = memo(function MarkdownContent({
  text,
}: {
  text: string
}) {
  return (
    <MarkdownGuard fallback={text}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={markdownComponents}
      >
        {text}
      </ReactMarkdown>
    </MarkdownGuard>
  )
})

/** Streaming 时按换行边界分割：已稳定行走 ReactMarkdown，未完成尾巴纯文本。 */
function StreamingMarkdown({ text }: { text: string }) {
  const lastNewline = text.lastIndexOf('\n')
  if (lastNewline === -1) {
    return (
      <>
        <span className="whitespace-pre-wrap break-words">{text}</span>
        <span className="ml-px inline-block animate-blink text-text-secondary motion-reduce:animate-none">
          ▋
        </span>
      </>
    )
  }
  const committed = text.slice(0, lastNewline + 1)
  const tail = text.slice(lastNewline + 1)
  return (
    <>
      <MarkdownContent text={committed} />
      <span className="whitespace-pre-wrap break-words">{tail}</span>
      <span className="ml-px inline-block animate-blink text-text-secondary motion-reduce:animate-none">
        ▋
      </span>
    </>
  )
}

function extractThinkingBlocks(text: string): {
  visibleText: string
  thinkingBlocks: string[]
} {
  if (typeof text !== 'string') return { visibleText: '', thinkingBlocks: [] }
  const thinkingBlocks: string[] = []
  const visibleText = text
    .replace(
      /<think-block>([\s\S]*?)<\/think-block>/gi,
      (_match, content: string) => {
        const normalized = content.trim()
        if (normalized && !thinkingBlocks.includes(normalized)) {
          thinkingBlocks.push(normalized)
        }
        return ''
      }
    )
    .trim()
  return { visibleText, thinkingBlocks }
}

function AssistantMessage({ block, reasoningText }: AssistantMessageProps) {
  const { visibleText, thinkingBlocks } = React.useMemo(() => {
    if (reasoningText) {
      return { visibleText: block.text, thinkingBlocks: [reasoningText] }
    }
    return extractThinkingBlocks(block.text)
  }, [block.text, reasoningText])
  const streaming = block.status === 'streaming'

  return (
    <div className="flex items-start gap-4 animate-message-enter max-sm:gap-3 motion-reduce:animate-none">
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
          {thinkingBlocks.map((block, index) => (
            <details
              key={`thinking-${index}`}
              className="mb-3.5 bg-transparent border-none rounded-0 overflow-visible group"
              open={streaming}
            >
              <summary className="inline-flex items-center gap-2 py-1 min-h-[24px] cursor-pointer select-none bg-transparent border-none rounded-0 text-text-secondary transition-opacity duration-150 ease-out text-sm font-medium list-none [&::-webkit-details-marker]:hidden hover:opacity-80">
                <span className="w-4 h-4 inline-flex items-center justify-center shrink-0 text-[13px] text-text-secondary">
                  <svg
                    width="16"
                    height="16"
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
                <span>Thinking</span>
                <span className="inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center text-text-secondary opacity-60 transition-transform duration-150 ease-out group-open:rotate-90">
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
              <div className="mb-3 ml-2 mt-2 border-l-2 border-border pl-4 overflow-wrap-anywhere text-sm leading-relaxed text-text-secondary prose-chat">
                {streaming ? (
                  <StreamingMarkdown text={block} />
                ) : (
                  <MarkdownContent text={block} />
                )}
              </div>
            </details>
          ))}
          {streaming ? (
            visibleText ? (
              <StreamingMarkdown text={visibleText} />
            ) : null
          ) : visibleText ? (
            <MarkdownContent text={visibleText} />
          ) : null}
        </div>
      </div>
    </div>
  )
}

export default memo(AssistantMessage)
