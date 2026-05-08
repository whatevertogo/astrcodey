import { memo } from 'react'
import type { ConversationBlock } from '../../services/types'
import { emptyStateSurface } from '../../lib/styles'

interface SystemNoteProps {
  block: Extract<ConversationBlock, { kind: 'systemNote' }>
}

function SystemNote({ block }: SystemNoteProps) {
  return (
    <div className={emptyStateSurface}>
      <div className="text-[13px] text-text-secondary">{block.text}</div>
    </div>
  )
}

export default memo(SystemNote)
