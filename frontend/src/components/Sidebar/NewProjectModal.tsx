import { useState, useCallback } from 'react'
import {
  overlay,
  dialogSurface,
  btnPrimary,
  btnSecondary,
  fieldInput,
  fieldButton,
} from '../../lib/styles'

interface NewProjectModalProps {
  onConfirm: (workingDir: string) => void
  onCancel: () => void
  canBrowse: boolean
  onSelectDirectory: () => Promise<string | null>
}

export default function NewProjectModal({
  onConfirm,
  onCancel,
  canBrowse,
  onSelectDirectory,
}: NewProjectModalProps) {
  const [path, setPath] = useState('')

  const handleSelectDirectory = useCallback(async () => {
    const dir = await onSelectDirectory()
    if (dir) setPath(dir)
  }, [onSelectDirectory])

  const handleSubmit = useCallback(() => {
    const trimmed = path.trim()
    if (!trimmed) return
    onConfirm(trimmed)
  }, [path, onConfirm])

  return (
    <div className={overlay} onClick={onCancel}>
      <div className={dialogSurface} onClick={(e) => e.stopPropagation()}>
        <h2 className="mb-4 text-[15px] font-semibold text-text-primary">
          新建项目
        </h2>
        <div className="mb-4">
          <label className="mb-1.5 block text-[13px] text-text-secondary">
            工作目录
          </label>
          <div className="flex gap-2">
            <input
              type="text"
              className={fieldInput}
              value={path}
              onChange={(e) => setPath(e.target.value)}
              placeholder="输入或选择目录路径..."
              onKeyDown={(e) => {
                if (e.key === 'Enter') handleSubmit()
              }}
            />
            {canBrowse && (
              <button
                type="button"
                className={fieldButton}
                onClick={() => void handleSelectDirectory()}
                style={{ width: 'auto', whiteSpace: 'nowrap' }}
              >
                浏览...
              </button>
            )}
          </div>
        </div>
        <div className="flex justify-end gap-2">
          <button type="button" className={btnSecondary} onClick={onCancel}>
            取消
          </button>
          <button
            type="button"
            className={btnPrimary}
            onClick={handleSubmit}
            disabled={!path.trim()}
          >
            创建
          </button>
        </div>
      </div>
    </div>
  )
}
