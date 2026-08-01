import { useCallback, useEffect, useMemo, useReducer, useState } from 'react'
import { useAppStore } from '../../../store/conversation'
import { pendingAskUserKey } from '../../../store/delta/applyDelta'
import { cn } from '../../../lib/utils'
import {
  rejectAskUserQuestion,
  respondAskUserQuestion,
} from '../../../services/api'
import type { ConversationBlock } from '../../../services/types'
import type { JsonRecord } from './helpers'
import {
  parseAskUserInput,
  parseAskUserOutput,
  type AskUserQuestion,
} from './askUser'

type QuestionState = {
  selectedLabels: string[]
  otherText: string
  useOther: boolean
}

type State = {
  index: number
  byQuestion: Record<string, QuestionState>
}

type Action =
  | { type: 'select'; question: string; label: string; multi: boolean }
  | { type: 'toggleOther'; question: string; on: boolean }
  | { type: 'setOther'; question: string; text: string }
  | { type: 'next'; max: number }
  | { type: 'prev' }

function emptyQuestionState(): QuestionState {
  return { selectedLabels: [], otherText: '', useOther: false }
}

function reducer(state: State, action: Action): State {
  switch (action.type) {
    case 'select': {
      const prev = state.byQuestion[action.question] ?? emptyQuestionState()
      const nextLabels = action.multi
        ? prev.selectedLabels.includes(action.label)
          ? prev.selectedLabels.filter((l) => l !== action.label)
          : [...prev.selectedLabels, action.label]
        : [action.label]
      return {
        ...state,
        byQuestion: {
          ...state.byQuestion,
          [action.question]: {
            ...prev,
            selectedLabels: nextLabels,
            useOther: false,
          },
        },
      }
    }
    case 'toggleOther': {
      const prev = state.byQuestion[action.question] ?? emptyQuestionState()
      return {
        ...state,
        byQuestion: {
          ...state.byQuestion,
          [action.question]: {
            ...prev,
            useOther: action.on,
            selectedLabels: action.on ? [] : prev.selectedLabels,
          },
        },
      }
    }
    case 'setOther': {
      const prev = state.byQuestion[action.question] ?? emptyQuestionState()
      return {
        ...state,
        byQuestion: {
          ...state.byQuestion,
          [action.question]: {
            ...prev,
            otherText: action.text,
            useOther: true,
            selectedLabels: [],
          },
        },
      }
    }
    case 'next':
      return { ...state, index: Math.min(state.index + 1, action.max) }
    case 'prev':
      return { ...state, index: Math.max(0, state.index - 1) }
    default:
      return state
  }
}

function answerForQuestion(
  q: AskUserQuestion,
  st: QuestionState
): string | null {
  if (st.useOther) {
    const t = st.otherText.trim()
    return t || null
  }
  if (q.multiSelect) {
    return st.selectedLabels.length > 0 ? st.selectedLabels.join(', ') : null
  }
  return st.selectedLabels[0] ?? null
}

function CompletedAnswers({
  answers,
  autoSelected,
}: {
  answers: Record<string, string>
  autoSelected?: boolean
}) {
  return (
    <div className="space-y-2">
      {autoSelected && (
        <p className="text-[12px] text-text-muted">
          超时未响应，已自动选择推荐选项
        </p>
      )}
      <ul className="space-y-2 text-[13px] text-text-secondary">
        {Object.entries(answers).map(([question, answer]) => (
          <li key={question}>
            <span className="text-text-muted">{question}</span>
            <span className="mx-2 text-text-muted">→</span>
            <span className="font-medium text-text-primary">{answer}</span>
          </li>
        ))}
      </ul>
    </div>
  )
}

export function AskUserCard({
  block,
  sessionId,
  args,
}: {
  block: Extract<ConversationBlock, { kind: 'toolCall' }>
  sessionId: string | null
  args: JsonRecord
}) {
  const pendingKey = sessionId ? pendingAskUserKey(sessionId, block.id) : null
  const pendingQuestion = useAppStore((state) =>
    pendingKey ? state.pendingAskUserQuestions[pendingKey] : undefined
  )
  const input = useMemo(
    () =>
      pendingQuestion
        ? {
            questions: pendingQuestion.questions,
            metadata: pendingQuestion.metadata,
          }
        : parseAskUserInput(block.argumentsJson ?? args),
    [args, block.argumentsJson, pendingQuestion]
  )
  const completed = useMemo(() => parseAskUserOutput(block.text), [block.text])
  const pending = pendingQuestion != null

  const [state, dispatch] = useReducer(reducer, {
    index: 0,
    byQuestion: {},
  })
  const [submitting, setSubmitting] = useState(false)
  const [submittedAnswers, setSubmittedAnswers] = useState<Record<
    string,
    string
  > | null>(null)
  const [submitError, setSubmitError] = useState<string | null>(null)
  const [clockMillis, setClockMillis] = useState(() => Date.now())

  const questions = input?.questions ?? completed?.questions ?? []
  const autoSelectAt = pendingQuestion?.autoSelectAt
  const remainingSecs =
    autoSelectAt === undefined
      ? null
      : Math.max(0, Math.ceil((autoSelectAt - clockMillis) / 1000))

  useEffect(() => {
    if (autoSelectAt === undefined) return
    const timer = window.setInterval(() => {
      setClockMillis(Date.now())
    }, 1000)
    return () => window.clearInterval(timer)
  }, [autoSelectAt])
  const current = questions[state.index]
  const qState = current
    ? (state.byQuestion[current.question] ?? emptyQuestionState())
    : emptyQuestionState()
  const canAdvanceCurrent = current
    ? answerForQuestion(current, qState) != null
    : false

  const allAnswers = useCallback((): Record<string, string> | null => {
    if (!input) return null
    const out: Record<string, string> = {}
    for (const q of input.questions) {
      const st = state.byQuestion[q.question] ?? emptyQuestionState()
      const ans = answerForQuestion(q, st)
      if (!ans) return null
      out[q.question] = ans
    }
    return out
  }, [input, state.byQuestion])

  const handleSubmit = useCallback(async () => {
    const answers = allAnswers()
    if (!answers || !sessionId) return
    setSubmitting(true)
    setSubmitError(null)
    try {
      await respondAskUserQuestion(sessionId, block.id, answers)
      setSubmittedAnswers(answers)
    } catch (e) {
      setSubmitError(e instanceof Error ? e.message : String(e))
    } finally {
      setSubmitting(false)
    }
  }, [allAnswers, sessionId, block.id])

  const handleReject = useCallback(async () => {
    if (!sessionId) return
    setSubmitting(true)
    setSubmitError(null)
    try {
      await rejectAskUserQuestion(sessionId, block.id)
    } catch (e) {
      setSubmitError(e instanceof Error ? e.message : String(e))
    } finally {
      setSubmitting(false)
    }
  }, [sessionId, block.id])

  const visibleCompletedAnswers = completed?.answers ?? submittedAnswers

  if (
    visibleCompletedAnswers &&
    Object.keys(visibleCompletedAnswers).length > 0
  ) {
    return (
      <div className="space-y-2">
        <p className="text-[12px] font-semibold uppercase tracking-wider text-text-muted">
          {completed?.answers ? '用户回答' : '已提交，等待继续'}
        </p>
        <CompletedAnswers
          answers={visibleCompletedAnswers}
          autoSelected={completed?.autoSelected}
        />
      </div>
    )
  }

  if (!input || questions.length === 0) {
    return <p className="text-[13px] text-text-muted">等待交互参数…</p>
  }

  if (!pending) {
    return (
      <p className="text-[13px] text-text-muted">{block.text || '(无输出)'}</p>
    )
  }

  return (
    <div className="space-y-4">
      {questions.length > 1 && (
        <p className="text-[11px] font-semibold uppercase tracking-wider text-text-muted">
          问题 {state.index + 1} / {questions.length}
        </p>
      )}

      {remainingSecs !== null && (
        <p className="text-[12px] text-text-muted">
          {remainingSecs > 0
            ? `${remainingSecs}s 后自动选择推荐选项`
            : '正在自动选择推荐选项…'}
        </p>
      )}

      {current && (
        <div className="space-y-3">
          <div>
            <span className="mb-2 inline-block rounded-md border border-border bg-surface px-2 py-0.5 text-[11px] font-semibold uppercase tracking-wider text-accent">
              {current.header}
            </span>
            <p className="text-[14px] font-medium text-text-primary">
              {current.question}
            </p>
          </div>

          <div className="flex flex-col gap-2">
            {current.options.map((opt) => {
              const selected = current.multiSelect
                ? qState.selectedLabels.includes(opt.label)
                : qState.selectedLabels[0] === opt.label && !qState.useOther
              return (
                <button
                  key={opt.label}
                  type="button"
                  onClick={() =>
                    dispatch({
                      type: 'select',
                      question: current.question,
                      label: opt.label,
                      multi: current.multiSelect === true,
                    })
                  }
                  className={cn(
                    'rounded-lg border px-3 py-2 text-left transition-colors',
                    selected
                      ? 'border-accent bg-accent/10'
                      : 'border-border bg-surface hover:border-accent/40'
                  )}
                >
                  <span className="flex items-center gap-2">
                    <span className="block text-[13px] font-medium text-text-primary">
                      {opt.label}
                    </span>
                    {opt.recommended && (
                      <span className="rounded-sm border border-accent/50 bg-accent/10 px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-accent">
                        推荐
                      </span>
                    )}
                  </span>
                  <span className="mt-0.5 block text-[12px] text-text-muted">
                    {opt.description}
                  </span>
                  {opt.preview && !current.multiSelect && selected && (
                    <pre className="mt-2 max-h-40 overflow-auto rounded-md bg-code-surface p-2 font-mono text-[11px] text-text-secondary">
                      {opt.preview}
                    </pre>
                  )}
                </button>
              )
            })}

            <div className="rounded-lg border border-dashed border-border p-3">
              <label className="flex cursor-pointer items-center gap-2 text-[12px] text-text-secondary">
                <input
                  type="checkbox"
                  checked={qState.useOther}
                  onChange={(e) =>
                    dispatch({
                      type: 'toggleOther',
                      question: current.question,
                      on: e.target.checked,
                    })
                  }
                />
                其他（自定义输入）
              </label>
              {qState.useOther && (
                <input
                  type="text"
                  className="mt-2 w-full rounded-md border border-border bg-panel-bg px-2 py-1.5 text-[13px] text-text-primary"
                  placeholder="输入你的回答…"
                  value={qState.otherText}
                  onChange={(e) =>
                    dispatch({
                      type: 'setOther',
                      question: current.question,
                      text: e.target.value,
                    })
                  }
                />
              )}
            </div>
          </div>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        {state.index > 0 && (
          <button
            type="button"
            className="rounded-md border border-border px-3 py-1.5 text-[12px] text-text-secondary hover:bg-surface"
            onClick={() => dispatch({ type: 'prev' })}
          >
            上一题
          </button>
        )}
        {state.index < questions.length - 1 ? (
          <button
            type="button"
            disabled={!canAdvanceCurrent}
            className="rounded-md bg-accent px-3 py-1.5 text-[12px] font-medium text-white disabled:opacity-40"
            onClick={() =>
              dispatch({ type: 'next', max: questions.length - 1 })
            }
          >
            下一题
          </button>
        ) : (
          <button
            type="button"
            disabled={!canAdvanceCurrent || !sessionId || submitting}
            className="rounded-md bg-accent px-3 py-1.5 text-[12px] font-medium text-white disabled:opacity-40"
            onClick={() => void handleSubmit()}
          >
            {submitting ? '提交中…' : '提交回答'}
          </button>
        )}
        <button
          type="button"
          disabled={!sessionId || submitting}
          className="rounded-md border border-border px-3 py-1.5 text-[12px] text-text-secondary hover:bg-surface disabled:opacity-40"
          onClick={() => void handleReject()}
        >
          拒绝
        </button>
      </div>
      {submitError && <p className="text-[12px] text-red-500">{submitError}</p>}
    </div>
  )
}
