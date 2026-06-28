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

  useKeybindings()
  const showHeroComposer = activeSessionId !== null && blocks.length === 0

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col overflow-hidden bg-panel-bg">
      <TopBar isSidebarOpen={isSidebarOpen} onToggleSidebar={onToggleSidebar} />
      {showHeroComposer ? (
        <main className="flex min-h-0 flex-1 flex-col items-center justify-center bg-panel-bg px-[var(--layout-page-padding-x)] pb-[18vh]">
          <h1 className="mb-14 max-w-[min(100%,920px)] text-center text-[36px] font-medium leading-tight text-text-primary">
            我们应该在{' '}
            {workingDir?.split(/[\\/]/).filter(Boolean).pop() ?? 'astrcodey'}{' '}
            中构建什么？
          </h1>
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
