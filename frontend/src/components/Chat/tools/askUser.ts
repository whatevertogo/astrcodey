import type {
  AskUserOption,
  AskUserQuestion,
  ConversationBlock,
  PendingAskUserQuestion,
} from '../../../services/types'

export type { AskUserOption, AskUserQuestion } from '../../../services/types'

export interface AskUserInput {
  questions: AskUserQuestion[]
  metadata?: { source?: string }
}

export interface AskUserOutput {
  questions: AskUserQuestion[]
  answers: Record<string, string>
  autoSelected?: boolean
}

export function remainingAutoSelectSeconds(
  pending: PendingAskUserQuestion,
  monotonicNow: number
): number | null {
  if (pending.autoSelectAt === undefined || pending.serverTime === undefined) {
    return null
  }
  const elapsed = Math.max(0, monotonicNow - pending.receivedAtMonotonic)
  return Math.max(
    0,
    Math.ceil((pending.autoSelectAt - pending.serverTime - elapsed) / 1000)
  )
}

type JsonRecord = Record<string, unknown>

function asRecord(value: unknown): JsonRecord {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? (value as JsonRecord)
    : {}
}

function arrayValue(source: JsonRecord, ...keys: string[]): unknown[] {
  for (const key of keys) {
    const value = source[key]
    if (Array.isArray(value)) return value
  }
  return []
}

function parseOption(raw: unknown): AskUserOption | null {
  const obj = asRecord(raw)
  if (typeof obj.label !== 'string' || typeof obj.description !== 'string') {
    return null
  }
  const preview =
    typeof obj.preview === 'string' && obj.preview.trim()
      ? obj.preview
      : undefined
  return {
    label: obj.label,
    description: obj.description,
    preview,
    recommended: obj.recommended === true,
  }
}

function parseQuestion(raw: unknown): AskUserQuestion | null {
  const obj = asRecord(raw)
  if (typeof obj.question !== 'string' || typeof obj.header !== 'string') {
    return null
  }
  const options = arrayValue(obj, 'options')
    .map(parseOption)
    .filter((o): o is AskUserOption => o != null)
  if (options.length < 2) return null
  return {
    question: obj.question,
    header: obj.header,
    options,
    multiSelect: obj.multiSelect === true,
  }
}

export function parseAskUserInput(
  args: JsonRecord | undefined
): AskUserInput | null {
  if (!args) return null
  const questions = arrayValue(args, 'questions')
    .map(parseQuestion)
    .filter((q): q is AskUserQuestion => q != null)
  if (questions.length === 0) return null
  const meta = asRecord(args.metadata)
  const source = typeof meta.source === 'string' ? meta.source : undefined
  return {
    questions,
    metadata: source ? { source } : undefined,
  }
}

export function parseAskUserOutput(text: string): AskUserOutput | null {
  const trimmed = text.trim()
  if (!trimmed.startsWith('{')) return null
  try {
    const obj = asRecord(JSON.parse(trimmed) as unknown)
    if (obj.status === 'awaiting_user_input') return null
    const answers = asRecord(obj.answers)
    const answerEntries = Object.entries(answers).filter(
      ([, v]) => typeof v === 'string'
    ) as [string, string][]
    if (answerEntries.length === 0) return null
    const questions = arrayValue(obj, 'questions')
      .map(parseQuestion)
      .filter((q): q is AskUserQuestion => q != null)
    return {
      questions,
      answers: Object.fromEntries(answerEntries),
      autoSelected: obj.autoSelected === true,
    }
  } catch {
    return null
  }
}

type AskUserBlock = Extract<ConversationBlock, { kind: 'toolCall' }>

export function isPendingAskUser(block: AskUserBlock): boolean {
  return block.name === 'askUser' && block.status === 'streaming'
}

export function askUserSummary(
  block: AskUserBlock,
  args: JsonRecord
): string | undefined {
  if (block.name !== 'askUser') return undefined
  const input = parseAskUserInput(args)
  if (!input) return undefined
  const first = input.questions[0]
  return [
    block.name,
    first.header,
    input.questions.length === 1
      ? '1 question'
      : `${input.questions.length} questions`,
  ].join(' · ')
}
