import { memo } from 'react'
import type { RenderSpec, RenderTone } from '../../types/render-spec'
import { MarkdownContent } from './MarkdownContent'
import { codeBlockShell, codeBlockContent } from '../../lib/styles'
import { cn } from '../../lib/utils'
import { DiffCodeLines } from './DiffCodeLines'

// ── Tone → CSS class ─────────────────────────────────────────────────────

function toneClass(tone?: RenderTone): string {
  switch (tone) {
    case 'muted':
      return 'text-text-muted'
    case 'accent':
      return 'text-accent-strong'
    case 'success':
      return 'text-success'
    case 'warning':
      return 'text-warning'
    case 'error':
      return 'text-danger'
    default:
      return ''
  }
}

// ── Recursive RenderSpec viewer ───────────────────────────────────────────

interface RenderSpecViewerProps {
  spec: RenderSpec
  className?: string
}

function RenderSpecViewerInner({ spec, className }: RenderSpecViewerProps) {
  switch (spec.type) {
    case 'text':
      return (
        <span className={cn(toneClass(spec.tone), className)}>{spec.text}</span>
      )

    case 'markdown':
      return (
        <div
          className={cn(
            'prose-chat min-w-0 max-w-full',
            toneClass(spec.tone),
            className
          )}
        >
          <MarkdownContent text={spec.text} />
        </div>
      )

    case 'box':
      return (
        <div
          className={cn(
            'space-y-3 rounded-xl border border-border bg-surface-soft p-3',
            className
          )}
        >
          {spec.title && (
            <div className="mb-2 text-[12px] font-semibold tracking-wide text-text-secondary uppercase">
              {spec.title}
            </div>
          )}
          {spec.children?.map((child, i) => (
            <RenderSpecViewerInner key={i} spec={child} />
          ))}
        </div>
      )

    case 'list': {
      const items = spec.items ?? []
      const Tag = spec.ordered ? 'ol' : 'ul'
      const allProgress =
        items.length > 0 && items.every((item) => item.type === 'progress')
      return (
        <Tag
          className={cn(
            'ml-4 space-y-2',
            spec.ordered
              ? 'list-decimal'
              : allProgress
                ? 'list-none'
                : 'list-disc',
            className
          )}
        >
          {items.map((item, i) => (
            <li key={i} className={toneClass(spec.tone)}>
              <RenderSpecViewerInner spec={item} />
            </li>
          ))}
        </Tag>
      )
    }

    case 'key_value':
      return (
        <dl className={cn('space-y-1 text-[13px]', className)}>
          {(spec.entries ?? []).map((entry, i) => (
            <div key={i} className="flex gap-2">
              <dt className="shrink-0 text-text-secondary">{entry.key}</dt>
              <dd className={toneClass(entry.tone)}>{entry.value}</dd>
            </div>
          ))}
        </dl>
      )

    case 'progress': {
      const pct = spec.value != null ? Math.round(spec.value * 100) : undefined
      return (
        <div className={cn('min-w-0 space-y-1 text-[13px]', className)}>
          <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5">
            <span className="min-w-0 wrap-break-word">{spec.label}</span>
            {spec.status && (
              <span className="shrink-0 text-text-muted">{spec.status}</span>
            )}
            {pct != null && (
              <span className="shrink-0 text-text-muted">{pct}%</span>
            )}
          </div>
          {spec.value != null && (
            <div className="h-1.5 overflow-hidden rounded-full bg-surface-muted">
              <div
                className="h-full rounded-full bg-accent-strong transition-[width] duration-300"
                style={{ width: `${pct}%` }}
              />
            </div>
          )}
        </div>
      )
    }

    case 'diff':
      return (
        <div className={cn(codeBlockShell, className)}>
          <pre
            className={cn(codeBlockContent, 'whitespace-pre')}
            children={
              <DiffCodeLines text={spec.text} lineClassName="-mx-4 px-4" />
            }
          />
        </div>
      )

    case 'code':
      return (
        <div className={cn(codeBlockShell, className)}>
          {spec.language && (
            <div className="flex items-center justify-between bg-code-surface px-4 pb-1 pt-2 text-xs text-code-label">
              {spec.language}
            </div>
          )}
          <pre
            className={codeBlockContent}
            children={<code>{spec.text}</code>}
          />
        </div>
      )

    case 'image_ref':
      return (
        <span className={cn('text-text-muted', className)}>
          [image: {spec.alt ?? spec.uri}]
        </span>
      )

    case 'raw_ansi_limited':
      return (
        <pre
          className={cn(
            'overflow-x-auto whitespace-pre font-mono text-[13px]',
            className
          )}
          children={spec.text}
        />
      )
  }
}

export const RenderSpecViewer = memo(RenderSpecViewerInner)
