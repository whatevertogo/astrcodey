import { useState } from 'react'
import { useAppStore } from '../../store/conversation'
import { cn } from '../../lib/utils'
import { ghostIconButton } from '../../lib/styles'
import { Icon } from '../ui/Icon'
import { IconButton } from '../ui/IconButton'

interface PendingMessagesPanelProps {
  onEdit: (text: string) => void
  canInject: boolean
}

export default function PendingMessagesPanel({
  onEdit,
  canInject,
}: PendingMessagesPanelProps) {
  const pendingMessages = useAppStore((s) => s.pendingMessages)
  const injectPendingMessage = useAppStore((s) => s.injectPendingMessage)
  const removePendingMessage = useAppStore((s) => s.removePendingMessage)
  const restorePendingMessage = useAppStore((s) => s.restorePendingMessage)
  const [expanded, setExpanded] = useState(true)

  const summary = `${pendingMessages.length} queued`

  if (pendingMessages.length === 0) {
    return null
  }

  return (
    <section className="mb-3">
      <button
        type="button"
        className="flex items-center gap-1.5 text-[12px] text-text-muted transition-colors hover:text-text-secondary"
        onClick={() => setExpanded((open) => !open)}
        aria-expanded={expanded}
      >
        <Icon
          name="chevron-down"
          size={12}
          className={cn(
            'transition-transform duration-150',
            !expanded && '-rotate-90'
          )}
        />
        <span>{summary}</span>
      </button>

      {expanded && (
        <ul className="mt-1.5 divide-y divide-border/80">
          {pendingMessages.map((message) => (
            <li
              key={message.id}
              className="group flex items-center gap-3 py-2.5"
            >
              <p className="min-w-0 flex-1 truncate text-[14px] leading-snug text-text-primary">
                {message.text}
              </p>
              <div className="flex shrink-0 items-center gap-0.5 opacity-60 transition-opacity group-hover:opacity-100">
                <IconButton
                  icon="edit"
                  label="编辑"
                  size={14}
                  className="rounded-md"
                  onClick={() => {
                    const text = restorePendingMessage(message.id)
                    if (text) onEdit(text)
                  }}
                />
                <button
                  type="button"
                  className={cn(ghostIconButton, 'rounded-md p-1')}
                  aria-label="Inject 到当前 turn"
                  title={
                    canInject
                      ? 'Inject 到当前 turn'
                      : 'Agent 未在运行，无法 inject'
                  }
                  disabled={!canInject}
                  onClick={() => void injectPendingMessage(message.id)}
                >
                  <Icon name="send" size={14} />
                </button>
                <IconButton
                  icon="trash"
                  label="删除"
                  size={14}
                  className="rounded-md"
                  onClick={() => removePendingMessage(message.id)}
                />
              </div>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}
