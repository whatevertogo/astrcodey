import { useEffect } from 'react';
import { useAppStore } from './store/conversation';
import Sidebar from './components/Sidebar/Sidebar';
import ChatView from './components/Chat/ChatView';
import ConnectingScreen from './components/ConnectingScreen';
import ErrorBoundary from './components/ErrorBoundary';

export default function App() {
  const connectionStatus = useAppStore((s) => s.connectionStatus);
  const initServer = useAppStore((s) => s.initServer);

  useEffect(() => {
    void initServer();
  }, [initServer]);

  if (connectionStatus !== 'connected') {
    return <ConnectingScreen />;
  }

  return (
    <ErrorBoundary>
      <div className="flex h-full min-h-0 overflow-hidden bg-app-bg text-text-primary">
        <div className="w-[260px] min-h-0 min-w-0 flex-none">
          <Sidebar />
        </div>
        <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
          <ChatView />
        </div>
      </div>
    </ErrorBoundary>
  );
}
