import { memo } from 'react'
import type { ConversationBlock } from '../../services/types'
import { errorSurface } from '../../lib/styles'

interface ErrorBlockProps {
  block: Extract<ConversationBlock, { kind: 'error' }>
}

function ErrorBlock({ block }: ErrorBlockProps) {
  return (
    <div className={errorSurface}>
      <div className="mb-1.5 text-[13px] font-semibold">错误</div>
      <div className="text-xs">{block.message}</div>
    </div>
  )
}

export default memo(ErrorBlock)
