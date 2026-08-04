import { useAppStore } from '../../store/conversation'
import { pendingAskUserKey } from '../../store/delta/applyDelta'
import { Icon } from '../ui'
import { AskUserCard } from './tools/AskUserCard'
import {
  pendingAskUserHasVisibleBlock,
  recoveredAskUserBlock,
} from './tools/askUser'

// 当前会话在重连时可能丢失 live tool block，此处用 pending 快照恢复问卷；
// 其他会话的待回答问题仍渲染成可点击横幅。
export function PendingAskUserBanner() {
  const pendingAskUserQuestions = useAppStore((s) => s.pendingAskUserQuestions)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const blocks = useAppStore((s) => s.blocks)
  const sessions = useAppStore((s) => s.sessions)
  const switchSession = useAppStore((s) => s.switchSession)

  const questions = Object.values(pendingAskUserQuestions)
  const recoveredCurrentSessionQuestions = questions.filter(
    (question) =>
      question.sessionId === activeSessionId &&
      !pendingAskUserHasVisibleBlock(blocks, question)
  )
  const otherSessionQuestions = questions.filter(
    (question) => question.sessionId !== activeSessionId
  )
  if (
    recoveredCurrentSessionQuestions.length === 0 &&
    otherSessionQuestions.length === 0
  ) {
    return null
  }

  const titleFor = (sessionId: string) =>
    sessions.find((session) => session.sessionId === sessionId)?.title ??
    sessionId.slice(0, 8)

  return (
    <div className="flex flex-col gap-1.5 border-b border-border bg-surface-soft/60 px-4 py-2">
      {recoveredCurrentSessionQuestions.map((question) => {
        const block = recoveredAskUserBlock(question)
        return (
          <div
            key={pendingAskUserKey(question.sessionId, question.callId)}
            className="rounded-lg border border-accent/30 bg-accent/5 p-3"
          >
            <AskUserCard
              block={block}
              sessionId={question.sessionId}
              args={block.argumentsJson ?? {}}
            />
          </div>
        )
      })}
      {otherSessionQuestions.map((question) => (
        <button
          key={pendingAskUserKey(question.sessionId, question.callId)}
          type="button"
          className="flex items-center gap-2 rounded-md border border-accent/30 bg-accent/5 px-3 py-1.5 text-left text-[13px] text-text-primary transition-colors hover:bg-accent/10"
          onClick={() => void switchSession(question.sessionId)}
        >
          <Icon name="spark" size={14} className="shrink-0 text-accent" />
          <span className="min-w-0 flex-1 truncate">
            会话「{titleFor(question.sessionId)}」有问题待回答：
            {question.questions[0]?.question}
          </span>
          <span className="shrink-0 text-[12px] text-text-muted">
            切换会话 →
          </span>
        </button>
      ))}
    </div>
  )
}
