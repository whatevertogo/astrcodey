import { useAppStore } from '../../store/conversation';
import MessageList from './MessageList';
import InputBar from './InputBar';
import TopBar from './TopBar';

export default function ChatView() {
  const blocks = useAppStore((s) => s.blocks);
  const activeSessionId = useAppStore((s) => s.activeSessionId);

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col overflow-hidden bg-panel-bg">
      <TopBar />
      <MessageList blocks={blocks} sessionId={activeSessionId} />
      <InputBar />
    </div>
  );
}
