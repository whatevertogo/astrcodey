import { useEffect } from 'react'
import { useAppStore } from './store/conversation'
import Sidebar from './components/Sidebar/Sidebar'
import ChatView from './components/Chat/ChatView'
import ConnectingScreen from './components/ConnectingScreen'
import ErrorBoundary from './components/ErrorBoundary'
import { useSidebarResize } from './hooks/useSidebarResize'

export default function App() {
  const connectionStatus = useAppStore((s) => s.connectionStatus)
  const initServer = useAppStore((s) => s.initServer)

  const { width, isOpen, toggle, onResizeStart, isResizing } =
    useSidebarResize()

  useEffect(() => {
    void initServer()
  }, [initServer])

  if (connectionStatus !== 'connected') {
    return <ConnectingScreen />
  }

  return (
    <ErrorBoundary>
      <div
        className={`flex h-full min-h-0 overflow-hidden bg-app-bg text-text-primary${isResizing ? ' select-none' : ''}`}
      >
        {isOpen && (
          <div className="min-h-0 min-w-0 flex-none" style={{ width }}>
            <Sidebar />
          </div>
        )}
        {isOpen && (
          <div
            className={`relative z-10 w-1.25 flex-none cursor-col-resize bg-transparent transition-colors duration-100 hover:bg-border-strong ${isResizing ? 'bg-border-strong' : ''}`}
            onPointerDown={onResizeStart}
          />
        )}
        <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
          <ChatView isSidebarOpen={isOpen} onToggleSidebar={toggle} />
        </div>
      </div>
    </ErrorBoundary>
  )
}
