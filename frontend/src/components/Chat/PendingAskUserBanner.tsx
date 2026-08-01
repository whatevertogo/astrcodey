import { useAppStore } from '../../store/conversation'
import { pendingAskUserKey } from '../../store/delta/applyDelta'
import { Icon } from '../ui'

// 跨会话提示条：当前激活会话之外的待回答问题。
// ask-user 的 pending/resolved 经全局通知通道广播，这里把"其他会话在等你"
// 渲染成可点击的横幅，点击切换到对应会话直接回答。
export function PendingAskUserBanner() {
  const pendingAskUserQuestions = useAppStore((s) => s.pendingAskUserQuestions)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const sessions = useAppStore((s) => s.sessions)
  const switchSession = useAppStore((s) => s.switchSession)

  const otherSessionQuestions = Object.values(pendingAskUserQuestions).filter(
    (question) => question.sessionId !== activeSessionId
  )
  if (otherSessionQuestions.length === 0) return null

  const titleFor = (sessionId: string) =>
    sessions.find((session) => session.sessionId === sessionId)?.title ??
    sessionId.slice(0, 8)

  return (
    <div className="flex flex-col gap-1.5 border-b border-border bg-surface-soft/60 px-4 py-2">
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
