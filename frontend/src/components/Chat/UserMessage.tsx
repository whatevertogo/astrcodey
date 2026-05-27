import { memo } from 'react'
import type { ConversationBlock } from '../../services/types'
import { cn } from '../../lib/utils'

interface UserMessageProps {
  block: Extract<ConversationBlock, { kind: 'user' }>
}

function UserMessage({ block }: UserMessageProps) {
  const images = block.images ?? []

  return (
    <div className="flex justify-end">
      <div
        className={cn(
          'max-w-[80%] rounded-2xl rounded-br-md bg-user-bubble px-4 py-3 text-[15px] leading-[1.7] text-text-primary prose-chat'
        )}
      >
        {block.text.trim() ? (
          <div className="whitespace-pre-wrap">{block.text}</div>
        ) : null}
        {images.length > 0 ? (
          <div
            className={cn(
              'grid gap-2',
              images.length > 1 ? 'mt-2 grid-cols-2' : 'mt-2 grid-cols-1'
            )}
          >
            {images.map((image) => (
              <figure
                key={`${block.id}:${image.filename}:${image.dataUrl.slice(0, 32)}`}
              >
                <img
                  src={image.dataUrl}
                  alt={image.filename}
                  className="max-h-64 w-full rounded-lg object-contain bg-black/5"
                  loading="lazy"
                />
                {image.filename ? (
                  <figcaption className="mt-1 truncate text-[11px] text-text-secondary">
                    {image.filename}
                  </figcaption>
                ) : null}
              </figure>
            ))}
          </div>
        ) : null}
      </div>
    </div>
  )
}

export default memo(UserMessage)
