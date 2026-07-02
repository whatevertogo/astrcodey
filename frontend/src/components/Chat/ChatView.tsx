import { useAppStore } from '../../store/conversation'
import MessageList from './MessageList'
import InputBar from './InputBar'
import TopBar from './TopBar'
import { useKeybindings } from '../../hooks/useKeybindings'

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
  const phase = useAppStore((s) => s.phase)

  useKeybindings()
  const showHeroComposer =
    activeSessionId !== null && blocks.length === 0 && phase === 'idle'

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col overflow-hidden bg-panel-bg">
      <TopBar isSidebarOpen={isSidebarOpen} onToggleSidebar={onToggleSidebar} />
      {showHeroComposer ? (
        <main className="flex min-h-0 flex-1 flex-col items-center justify-center bg-panel-bg px-[var(--layout-page-padding-x)] pb-[14vh]">
          <div className="flex w-full max-w-[var(--layout-content-max-width)] flex-col items-center">
            <h1 className="mb-10 w-full text-center text-[34px] font-medium leading-tight text-text-primary">
              我们应该在{' '}
              {workingDir?.split(/[\\/]/).filter(Boolean).pop() ?? 'astrcodey'}{' '}
              中构建什么？
            </h1>
            <InputBar presentation="hero" />
          </div>
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
