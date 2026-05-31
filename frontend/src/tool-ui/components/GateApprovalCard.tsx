import { useState } from 'react'
import { cn } from '../../lib/utils'
import { submitToolGateApproval } from '../../services/api'
import { stringValue, type JsonRecord } from '../../components/Chat/tools/helpers'

type GateApprovalMeta = {
  pending?: boolean
  prompt?: string
  ruleKey?: string
}

function readGateApproval(
  metadata: Record<string, unknown> | undefined
): GateApprovalMeta | null {
  const raw = metadata?.toolGateApproval
  if (!raw || typeof raw !== 'object') return null
  const gate = raw as Record<string, unknown>
  if (gate.pending === false) return null
  return {
    pending: gate.pending !== false,
    prompt: typeof gate.prompt === 'string' ? gate.prompt : undefined,
    ruleKey: typeof gate.ruleKey === 'string' ? gate.ruleKey : undefined,
  }
}

function commandFromPrompt(prompt?: string): string | undefined {
  if (!prompt) return undefined
  const lines = prompt.split('\n')
  if (lines.length <= 1) return undefined
  const body = lines.slice(1).join('\n').trim()
  return body || undefined
}

function approvalHeadline(toolName: string, prompt?: string): string {
  const firstLine = prompt?.split('\n')[0]?.trim()
  if (firstLine === 'Run shell command?') return '执行 Shell 命令需要你的确认'
  if (firstLine?.endsWith('?')) return firstLine
  return `${toolName} 需要你的确认`
}

function resolveCommand(
  args: JsonRecord,
  prompt?: string
): string | undefined {
  const fromArgs = stringValue(args, 'command')
  if (fromArgs) return fromArgs
  return commandFromPrompt(prompt)
}

function resolveIntent(args: JsonRecord): string | undefined {
  const intent = stringValue(args, 'intent')
  return intent || undefined
}

const primaryButton =
  'rounded-lg bg-text-primary px-3 py-1.5 text-[12px] font-medium text-white transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-40'
const secondaryButton =
  'rounded-lg border border-border bg-surface px-3 py-1.5 text-[12px] font-medium text-text-secondary transition-colors hover:border-border-strong hover:text-text-primary disabled:cursor-not-allowed disabled:opacity-40'
const dangerButton =
  'rounded-lg border border-danger/20 bg-surface px-3 py-1.5 text-[12px] font-medium text-danger transition-colors hover:border-danger/40 hover:bg-danger-soft/30 disabled:cursor-not-allowed disabled:opacity-40'

export function GateApprovalCard({
  sessionId,
  callId,
  toolName,
  metadata,
  args = {},
}: {
  sessionId: string
  callId: string
  toolName: string
  metadata: Record<string, unknown> | undefined
  args?: JsonRecord
}) {
  const gate = readGateApproval(metadata)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  if (!gate?.pending) return null

  const command = resolveCommand(args, gate.prompt)
  const intent = resolveIntent(args)
  const headline = approvalHeadline(toolName, gate.prompt)

  async function decide(
    decision: 'allow_once' | 'deny_once' | 'allow_always' | 'deny_always'
  ) {
    setBusy(true)
    setError(null)
    try {
      await submitToolGateApproval(sessionId, callId, decision)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-4">
      <div className="space-y-2">
        <span className="inline-block rounded-md border border-warning/30 bg-warning-soft px-2 py-0.5 text-[11px] font-semibold uppercase tracking-wider text-warning">
          待审批
        </span>
        <p className="text-[14px] font-medium text-text-primary">{headline}</p>
        {intent ? (
          <p className="text-[12px] text-text-muted">{intent}</p>
        ) : null}
      </div>

      {command ? (
        <pre className="overflow-x-auto rounded-lg border border-border bg-code-surface px-3 py-2.5 font-mono text-[12.5px] leading-relaxed text-code-text">
          <span className="select-none text-text-muted">$ </span>
          <span className="wrap-break-word">{command}</span>
        </pre>
      ) : gate.prompt ? (
        <p className="text-[13px] text-text-secondary">{gate.prompt}</p>
      ) : null}

      {error ? (
        <p className={cn('text-[12px] text-danger')}>{error}</p>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          disabled={busy}
          className={primaryButton}
          onClick={() => void decide('allow_once')}
        >
          允许一次
        </button>
        <button
          type="button"
          disabled={busy}
          className={secondaryButton}
          onClick={() => void decide('allow_always')}
        >
          始终允许
        </button>
        <button
          type="button"
          disabled={busy}
          className={dangerButton}
          onClick={() => void decide('deny_once')}
        >
          拒绝
        </button>
      </div>
    </div>
  )
}

export { readGateApproval }
