import type { ReactNode } from 'react'
import type { ToolUiContext } from './types'
import { QuestionnaireApprovalCard } from './components/QuestionnaireApprovalCard'
import {
  isAwaitingUserInput,
  parseAskUserInput,
  parseAskUserOutput,
} from './components/questionnaireTypes'
import { readToolUi, readToolUiPhase } from './wire'

/**
 * 按后端投影的 `metadata.toolUi` 选择宿主内置组件。
 * 扩展在 Rust `Registrar::tool_ui` 注册，前端不维护 toolName → UI 表。
 */
export function renderToolApprovalUi(ctx: ToolUiContext): ReactNode | null {
  const wire = readToolUi(ctx.meta)
  const phase = readToolUiPhase(ctx.meta)
  const approval = wire?.approval
  if (!approval || (phase && phase !== 'approval')) return null

  if (approval.kind === 'builtin' && approval.variant === 'questionnaire') {
    return <QuestionnaireApprovalCard ctx={ctx} />
  }

  return null
}

export function toolApprovalSummary(ctx: ToolUiContext): string | undefined {
  const wire = readToolUi(ctx.meta)
  if (
    wire?.approval?.kind === 'builtin' &&
    wire.approval.variant === 'questionnaire'
  ) {
    const args = ctx.block.argumentsJson ?? ctx.args
    const questions = args?.questions
    if (Array.isArray(questions) && questions.length > 0) {
      const first = questions[0] as { header?: string }
      const n = questions.length
      return [
        ctx.block.name,
        first?.header,
        n > 1 ? `${n} questions` : '1 question',
      ]
        .filter(Boolean)
        .join(' · ')
    }
  }
  return undefined
}

export function toolApprovalShouldAutoExpand(ctx: ToolUiContext): boolean {
  const wire = readToolUi(ctx.meta)
  if (
    wire?.approval?.kind !== 'builtin' ||
    wire.approval.variant !== 'questionnaire'
  ) {
    return false
  }
  return (
    ctx.block.status === 'streaming' ||
    (typeof ctx.block.text === 'string' &&
      ctx.block.text.includes('awaiting_user_input'))
  )
}

/** askUser 等问卷 UI 是否仍在等待用户提交。 */
export function toolApprovalPending(ctx: ToolUiContext): boolean {
  const wire = readToolUi(ctx.meta)
  if (
    wire?.approval?.kind !== 'builtin' ||
    wire.approval.variant !== 'questionnaire'
  ) {
    return false
  }
  if (ctx.block.status === 'error') return false
  const completed = parseAskUserOutput(ctx.block.text)
  if (completed?.answers && Object.keys(completed.answers).length > 0) {
    return false
  }
  const input = parseAskUserInput(ctx.block.argumentsJson ?? ctx.args)
  if (!input || input.questions.length === 0) return false
  return ctx.block.status === 'streaming' || isAwaitingUserInput(ctx.block.text)
}
