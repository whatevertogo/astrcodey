import { memo, useMemo } from 'react'
import type { ConversationBlock } from '../../services/types'
import { cn } from '../../lib/utils'
import { MarkdownContent } from './MarkdownContent'

interface UserMessageProps {
  block: Extract<ConversationBlock, { kind: 'user' }>
}

function UserMessage({ block }: UserMessageProps) {
  const imageSources = useMemo(
    () =>
      (block.attachments ?? [])
        .filter((attachment) => attachment.mediaType.startsWith('image/'))
        .map(
          (attachment) =>
            `data:${attachment.mediaType};base64,${attachment.content}`
        ),
    [block.attachments]
  )

  return (
    <div className="flex justify-end">
      <div
        className={cn(
          'max-w-[85%] rounded-2xl rounded-br-md border border-user-bubble-border',
          'bg-user-bubble px-4 py-3 text-[15px] leading-[1.65] text-text-primary prose-chat'
        )}
      >
        {imageSources.length > 0 && (
          <div className="mb-2 flex flex-wrap gap-2">
            {imageSources.map((src, index) => (
              <img
                key={`${block.id}-image-${index}`}
                src={src}
                alt=""
                className="h-16 w-16 rounded-lg border border-border object-cover"
              />
            ))}
          </div>
        )}
        {block.text.trim() ? <MarkdownContent text={block.text} /> : null}
      </div>
    </div>
  )
}

export default memo(UserMessage)
