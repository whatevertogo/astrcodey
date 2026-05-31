import { memo, useState, type ReactNode } from 'react'
import type { ConversationBlock } from '../../services/types'
import { cn } from '../../lib/utils'
import {
  extractRenderSpec,
  extractRenderSummary,
} from '../../types/render-spec'
import {
  renderToolApprovalUi,
  toolApprovalShouldAutoExpand,
  toolApprovalSummary,
  type ToolUiContext,
} from '../../tool-ui'
import { GateApprovalCard, readGateApproval } from '../../tool-ui/components/GateApprovalCard'
import { chevronIcon, toolPanelPaddingX, toolPanelScrollViewport } from '../../lib/styles'
import { RenderSpecViewer } from './RenderSpecViewer'
import './tools/builtinRenderers'
import {
  getToolRenderer,
  type ToolRenderer,
  type ToolRendererContext,
} from './tools/registry'
import { compactLine, statusLabel, toolArgs, toolMeta } from './tools/helpers'
import {
  buildStreamingAgentSpec,
  DefaultToolDetails,
  StatusIndicatorDot,
} from './tools/shared'
import { Icon } from '../ui/Icon'

interface ToolCallBlockProps {
  block: Extract<ConversationBlock, { kind: 'toolCall' }>
  sessionId: string | null
}

function ToolDetails({
  toolContext,
  approvalUi,
  renderer,
}: {
  toolContext: ToolRendererContext
  approvalUi: ReactNode | null
  renderer?: ToolRenderer
}) {
  if (toolContext.renderSpec) {
    return <RenderSpecViewer spec={toolContext.renderSpec} />
  }
  if (toolContext.agentSpec) {
    return <RenderSpecViewer spec={toolContext.agentSpec} />
  }
  if (approvalUi) return approvalUi
  const rendered = renderer?.render?.(toolContext)
  if (rendered != null) return rendered
  return <DefaultToolDetails block={toolContext.block} />
}

function ToolCallBlock({ block, sessionId }: ToolCallBlockProps) {
  const [isOpen, setIsOpen] = useState(false)
  const args = toolArgs(block)
  const meta = toolMeta(block)

  const renderSpec = extractRenderSpec(block.metadata)
  const agentSpec =
    block.name === 'agent' && block.argumentsJson && !renderSpec
      ? buildStreamingAgentSpec(block.argumentsJson)
      : undefined

  const toolUiCtx: ToolUiContext = {
    block,
    sessionId,
    args,
    meta,
    renderSpec,
  }
  const approvalUi = renderToolApprovalUi(toolUiCtx)

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
      toolApprovalSummary(toolUiCtx) ||
      renderer?.summary?.(context) ||
      block.arguments ||
      block.text ||
      (block.status === 'streaming' ? '等待输出...' : '(无输出)')
  )

  const gateApproval = readGateApproval(block.metadata)
  const gatePending = gateApproval?.pending === true
  const autoExpand =
    toolApprovalShouldAutoExpand(toolUiCtx) || gatePending

  const displayStatus = gatePending ? '待审批' : statusLabel(block.status)

  return (
    <details
      className="group mb-1 ml-[var(--layout-assistant-indent)] block min-w-0 max-w-full animate-block-enter motion-reduce:animate-none"
      open={block.status === 'error' || isOpen || !!agentSpec || autoExpand}
      onToggle={(e) => setIsOpen(e.currentTarget.open)}
    >
      <summary className="flex min-w-0 cursor-pointer list-none items-center gap-3 py-2 font-mono text-[13px] leading-relaxed text-text-secondary select-none hover:opacity-90 [&::-webkit-details-marker]:hidden">
        <span className="inline-flex shrink-0 items-center gap-1.5 rounded-md border border-border bg-surface px-2 py-0.5 font-mono text-[11px] font-semibold uppercase tracking-wider text-text-secondary">
          <StatusIndicatorDot
            status={block.status}
            pendingApproval={gatePending}
          />
          {block.name}
        </span>
        <span
          className="block min-w-0 flex-1 overflow-hidden text-ellipsis whitespace-nowrap font-mono text-[12.5px] text-text-secondary/85 opacity-90"
          title={summaryLine}
        >
          {summaryLine}
        </span>
        <span
          className={cn(
            'shrink-0 text-[11px] font-semibold uppercase tracking-wider',
            gatePending ? 'text-warning' : 'text-text-muted'
          )}
        >
          {displayStatus}
        </span>
        <span className={chevronIcon}>
          <Icon name="chevron-right" size={14} />
        </span>
      </summary>
      <div className="mt-1.5 min-w-0 overflow-hidden rounded-xl border border-border bg-code-surface shadow-soft">
        <div className={toolPanelScrollViewport}>
          <div className={cn(toolPanelPaddingX, 'py-3')}>
            {gatePending && sessionId ? (
              <GateApprovalCard
                sessionId={sessionId}
                callId={block.id}
                toolName={block.name}
                metadata={block.metadata}
                args={args}
              />
            ) : (
              <ToolDetails
                toolContext={context}
                approvalUi={approvalUi}
                renderer={renderer}
              />
            )}
          </div>
        </div>
      </div>
    </details>
  )
}

export default memo(ToolCallBlock)
