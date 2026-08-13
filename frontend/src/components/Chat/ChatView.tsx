import { useAppStore } from '../../store/conversation'
import MessageList from './MessageList'
import InputBar from './InputBar'
import TopBar from './TopBar'
import { PendingAskUserBanner } from './PendingAskUserBanner'
import { useKeybindings } from '../../hooks/useKeybindings'
import { Icon } from '../ui'
import { effectiveConversationPhase } from '../../store/phaseHelpers'

interface ChatViewProps {
  isSidebarOpen: boolean
  onToggleSidebar: () => void
}

export default function ChatView({
  isSidebarOpen,
  onToggleSidebar,
}: ChatViewProps) {
  const blocks = useAppStore((s) => s.blocks)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const workingDir = useAppStore((s) => s.workingDir)
  const phase = useAppStore((state) =>
    effectiveConversationPhase(state.control, state.compactSubmitting)
  )

  useKeybindings()
  const showHeroComposer =
    activeSessionId !== null && blocks.length === 0 && phase === 'idle'

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col overflow-hidden bg-panel-bg">
      <TopBar isSidebarOpen={isSidebarOpen} onToggleSidebar={onToggleSidebar} />
      <PendingAskUserBanner />
      {showHeroComposer ? (
        <main className="flex min-h-0 flex-1 flex-col bg-panel-bg px-[var(--layout-page-padding-x)] pb-5">
          <div className="flex min-h-0 flex-1 items-center justify-center">
            <div className="flex flex-col items-center">
              <Icon
                name="spark"
                size={52}
                className="mb-6 text-text-muted/60"
              />
              <h1 className="text-center text-[clamp(28px,2.6vw,38px)] font-normal leading-tight tracking-[-0.025em] text-text-primary">
                要在{' '}
                <span className="decoration-text-muted/60 underline decoration-dotted underline-offset-[6px]">
                  {workingDir?.split(/[\\/]/).filter(Boolean).pop() ??
                    'astrcodey'}
                </span>{' '}
                内开发什么？
              </h1>
            </div>
          </div>
          <InputBar presentation="hero" />
        </main>
      ) : (
        <>
          <MessageList blocks={blocks} sessionId={activeSessionId} />
          <InputBar presentation="docked" />
        </>
      )}
    </div>
  )
}
