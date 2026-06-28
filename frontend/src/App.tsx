import { useEffect, useState } from 'react'
import { useAppStore } from './store/conversation'
import Sidebar from './components/Sidebar/Sidebar'
import ChatView from './components/Chat/ChatView'
import PluginsPage from './components/Plugins/PluginsPage'
import SettingsPage from './components/Settings/SettingsPage'
import ConnectingScreen from './components/ConnectingScreen'
import ErrorBoundary from './components/ErrorBoundary'
import TransientHintDialog from './components/TransientHintDialog'
import { useSidebarResize } from './hooks/useSidebarResize'

export type MainView = 'chat' | 'plugins' | 'settings'

export default function App() {
  const connectionStatus = useAppStore((s) => s.connectionStatus)
  const initServer = useAppStore((s) => s.initServer)
  const [mainView, setMainView] = useState<MainView>('chat')

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
            <Sidebar
              activeView={mainView}
              onToggleSidebar={toggle}
              onOpenChat={() => setMainView('chat')}
              onOpenPlugins={() => setMainView('plugins')}
              onOpenSettings={() => setMainView('settings')}
            />
          </div>
        )}
        {isOpen && (
          <div
            className={`relative z-10 w-px flex-none cursor-col-resize bg-border transition-colors duration-100 hover:bg-border-strong ${isResizing ? 'bg-border-strong' : ''}`}
            onPointerDown={onResizeStart}
          />
        )}
        <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
          {mainView === 'plugins' ? (
            <PluginsPage
              isSidebarOpen={isOpen}
              onToggleSidebar={toggle}
              onOpenSettings={() => setMainView('settings')}
            />
          ) : mainView === 'settings' ? (
            <SettingsPage
              isSidebarOpen={isOpen}
              onToggleSidebar={toggle}
              onOpenPlugins={() => setMainView('plugins')}
            />
          ) : (
            <ChatView isSidebarOpen={isOpen} onToggleSidebar={toggle} />
          )}
          <TransientHintDialog />
        </div>
      </div>
    </ErrorBoundary>
  )
}
