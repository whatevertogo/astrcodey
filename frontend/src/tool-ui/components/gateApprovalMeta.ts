export type GateApprovalMeta = {
  pending?: boolean
  prompt?: string
  ruleKey?: string
}

export function readGateApproval(
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
