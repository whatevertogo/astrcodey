import { memo } from 'react'
import type { ConversationBlock } from '../../services/types'
import { cn } from '../../lib/utils'

interface UserMessageProps {
  block: Extract<ConversationBlock, { kind: 'user' }>
}

function UserMessage({ block }: UserMessageProps) {
  return (
    <div className="flex justify-end">
      <div
        className={cn(
          'max-w-[80%] rounded-2xl rounded-br-md bg-user-bubble px-4 py-3 text-[15px] leading-[1.7] text-text-primary prose-chat'
        )}
      >
        {block.text}
      </div>
    </div>
  )
}

export default memo(UserMessage)
