import React, { memo, useCallback, useState } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import {
  codeBlockContent,
  codeBlockHeader,
  codeBlockShell,
  ghostIconButton,
} from '../../lib/styles'
import { cn } from '../../lib/utils'

function CopyButton({ code }: { code: string }) {
  const [copied, setCopied] = useState(false)
  const handleCopy = useCallback(() => {
    void navigator.clipboard.writeText(code).then(() => {
      setCopied(true)
      window.setTimeout(() => setCopied(false), 2000)
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
            <polyline points="20 6 9 17 4 12" />
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
            <rect x="9" y="9" width="13" height="13" rx="2" ry="2" />
            <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
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

  const codeText = String(children).trim()
  return (
    <div className={codeBlockShell}>
      <div className={codeBlockHeader}>
        <span className="text-xs lowercase">{language || 'text'}</span>
        <CopyButton code={codeText} />
      </div>
      <pre
        className={codeBlockContent}
        {...props}
        children={<code className={className}>{codeText}</code>}
      />
    </div>
  )
}

function ExternalLink({
  href,
  children,
  ...rest
}: React.ComponentPropsWithoutRef<'a'>) {
  if (!href) return <span>{children}</span>
  const isExternal = /^https?:\/\//i.test(href)
  return (
    <a
      href={href}
      {...rest}
      {...(isExternal
        ? { target: '_blank', rel: 'noopener noreferrer' }
        : undefined)}
    >
      {children}
    </a>
  )
}

const markdownComponents = {
  pre: ({ children }: React.PropsWithChildren) => <>{children}</>,
  code: CodeBlockRenderer as React.ComponentType<
    React.ComponentPropsWithoutRef<'code'>
  >,
  a: ExternalLink,
  img: ({ src, alt, ...rest }: React.ComponentPropsWithoutRef<'img'>) =>
    src ? <img src={src} alt={alt} {...rest} /> : null,
}

function MarkdownRenderer({ text }: { text: string }) {
  return (
    <ReactMarkdown remarkPlugins={[remarkGfm]} components={markdownComponents}>
      {text}
    </ReactMarkdown>
  )
}

export default memo(MarkdownRenderer)
