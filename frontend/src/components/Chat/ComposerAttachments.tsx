import { Icon } from '../ui'
import { cn } from '../../lib/utils'
import type { PromptAttachment } from '../../services/types'

interface ComposerAttachmentsProps {
  attachments: PromptAttachment[]
  onRemove: (id: string) => void
}

export default function ComposerAttachments({
  attachments,
  onRemove,
}: ComposerAttachmentsProps) {
  if (attachments.length === 0) return null

  return (
    <div className="mb-2 flex flex-wrap gap-2">
      {attachments.map((attachment) => (
        <div
          key={attachment.id}
          className="group relative h-14 w-14 shrink-0 overflow-hidden rounded-lg border border-border bg-surface-muted"
        >
          <img
            src={attachment.previewUrl}
            alt={attachment.filename}
            className="h-full w-full object-cover"
          />
          <button
            type="button"
            className={cn(
              'absolute right-0.5 top-0.5 inline-flex h-5 w-5 items-center justify-center',
              'rounded-md bg-panel-bg/90 text-text-muted opacity-0 transition-opacity',
              'group-hover:opacity-100 hover:text-text-primary'
            )}
            onClick={() => onRemove(attachment.id)}
            aria-label={`移除 ${attachment.filename}`}
          >
            <Icon name="close" size={12} />
          </button>
        </div>
      ))}
    </div>
  )
}
