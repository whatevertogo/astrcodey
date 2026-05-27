import { useRef, useState, useCallback, useEffect } from 'react'
import { useAppStore } from '../../store/conversation'
import {
  composerShell,
  composerSubmitButton,
  composerInterruptButton,
} from '../../lib/styles'
import { cn } from '../../lib/utils'
import ModelSelector from './ModelSelector'
import CommandSelector from './CommandSelector'
import * as api from '../../services/api'
import type { SlashCommandInfo, PromptAttachment } from '../../services/types'

function isExecutionPhase(phase: string): boolean {
  return (
    phase === 'thinking' || phase === 'streaming' || phase === 'calling_tool'
  )
}

interface PendingImage {
  id: string
  filename: string
  previewUrl: string
  attachment: PromptAttachment
}

async function fileToPromptAttachment(file: File): Promise<PromptAttachment> {
  const dataUrl = await new Promise<string>((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve(String(reader.result ?? ''))
    reader.onerror = () => reject(reader.error ?? new Error('read failed'))
    reader.readAsDataURL(file)
  })
  const comma = dataUrl.indexOf(',')
  const content = comma >= 0 ? dataUrl.slice(comma + 1) : dataUrl
  return {
    filename: file.name || 'image',
    content,
    mediaType: file.type || 'image/png',
  }
}

export default function InputBar() {
  const submitPrompt = useAppStore((s) => s.submitPrompt)
  const abortCurrentTurn = useAppStore((s) => s.abortCurrentTurn)
  const phase = useAppStore((s) => s.phase)
  const workingDir = useAppStore((s) => s.workingDir)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const modelRefreshKey = useAppStore((s) => s.modelRefreshKey)
  const bumpModelRefreshKey = useAppStore((s) => s.bumpModelRefreshKey)
  const compactSubmitting = useAppStore((s) => s.compactSubmitting)
  const statusItems = useAppStore((s) => s.statusItems)
  const queuedMessages = useAppStore((s) => s.queuedMessages)

  const [value, setValue] = useState('')
  const [pendingImages, setPendingImages] = useState<PendingImage[]>([])
  const [isComposing, setIsComposing] = useState(false)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const isBusy = isExecutionPhase(phase) || compactSubmitting
  const canSubmit = !!activeSessionId && !compactSubmitting
  const hasInput = value.trim().length > 0 || pendingImages.length > 0

  // Abort 防抖：防止快速多次点击
  const abortDebounceRef = useRef<number | null>(null)
  const abortInProgressRef = useRef(false)

  // ── slash command panel state ──
  const [slashTriggerVisible, setSlashTriggerVisible] = useState(false)
  const [slashQuery, setSlashQuery] = useState('')
  const [slashOptions, setSlashOptions] = useState<SlashCommandInfo[]>([])
  const [slashLoading, setSlashLoading] = useState(false)
  const slashTriggerStartRef = useRef(0)
  const slashTriggerEndRef = useRef(0)
  const slashAbortRef = useRef<AbortController | null>(null)

  const closeSlashTrigger = useCallback(() => {
    setSlashTriggerVisible(false)
    setSlashQuery('')
    setSlashOptions([])
    setSlashLoading(false)
    slashAbortRef.current?.abort()
    slashAbortRef.current = null
  }, [])

  /** 在当前行找到光标位置的 `/` 触发上下文 */
  function findSlashTrigger(
    currentValue: string,
    cursorPos: number
  ): { triggerStart: number; triggerEnd: number; query: string } | null {
    const lineStart = Math.max(
      0,
      currentValue.lastIndexOf('\n', cursorPos - 1) + 1
    )
    const segment = currentValue.slice(lineStart, cursorPos)
    const slashIdx = segment.lastIndexOf('/')
    if (slashIdx === -1) return null

    const beforeSlash = slashIdx === 0 ? '' : segment[slashIdx - 1]
    if (beforeSlash !== ' ' && slashIdx !== 0) return null

    const afterSlash = segment.slice(slashIdx + 1)
    if (/\s/.test(afterSlash)) return null

    return {
      triggerStart: lineStart + slashIdx,
      triggerEnd: cursorPos,
      query: afterSlash,
    }
  }

  const updateSlashTrigger = useCallback(
    (currentValue: string, cursorPos: number) => {
      if (!activeSessionId) return

      const trigger = findSlashTrigger(currentValue, cursorPos)
      if (trigger) {
        slashTriggerStartRef.current = trigger.triggerStart
        slashTriggerEndRef.current = trigger.triggerEnd
        setSlashQuery(trigger.query)
        if (!slashTriggerVisible) {
          setSlashLoading(true)
          setSlashTriggerVisible(true)
        }
        return
      }

      if (slashTriggerVisible) {
        closeSlashTrigger()
      }
    },
    [activeSessionId, slashTriggerVisible, closeSlashTrigger]
  )

  // ── fetch commands when panel opens ──
  useEffect(() => {
    if (!slashTriggerVisible || !activeSessionId) return

    slashAbortRef.current?.abort()
    const controller = new AbortController()
    slashAbortRef.current = controller

    api
      .listCommands(activeSessionId)
      .then((res) => {
        if (controller.signal.aborted) return
        setSlashOptions(res.commands)
        setSlashLoading(false)
        // 初始化状态栏项到 store
        if (res.statusItems.length > 0) {
          const items: Record<string, string> = {}
          for (const item of res.statusItems) {
            items[item.id] = item.text
          }
          useAppStore.setState({ statusItems: items })
        }
      })
      .catch((err) => {
        if (controller.signal.aborted) return
        console.error('[CommandSelector] 获取命令列表失败:', err)
        setSlashOptions([])
        setSlashLoading(false)
      })

    return () => {
      controller.abort()
    }
  }, [activeSessionId, slashTriggerVisible])

  const handleSlashCommandSelect = useCallback(
    (option: SlashCommandInfo) => {
      const before = value.slice(0, slashTriggerStartRef.current)
      const after = value.slice(slashTriggerEndRef.current)
      const insertText = `/${option.name}`
      const nextValue = `${before}${insertText} ${after}`
      setValue(nextValue)
      closeSlashTrigger()

      requestAnimationFrame(() => {
        const textarea = textareaRef.current
        if (!textarea) return
        const nextCursor = before.length + insertText.length + 1
        textarea.focus()
        textarea.setSelectionRange(nextCursor, nextCursor)
        textarea.style.height = 'auto'
        textarea.style.height = `${Math.min(textarea.scrollHeight, 200)}px`
      })
    },
    [closeSlashTrigger, value]
  )

  const handleInput = useCallback(
    (event: React.ChangeEvent<HTMLTextAreaElement>) => {
      const nextValue = event.target.value
      setValue(nextValue)
      const textarea = textareaRef.current
      if (!textarea) return
      textarea.style.height = 'auto'
      textarea.style.height = `${Math.min(textarea.scrollHeight, 200)}px`
      updateSlashTrigger(nextValue, textarea.selectionStart)
    },
    [updateSlashTrigger]
  )

  const handleCursorActivity = useCallback(() => {
    const textarea = textareaRef.current
    if (!textarea) return
    updateSlashTrigger(value, textarea.selectionStart)
  }, [updateSlashTrigger, value])

  const addImageFiles = useCallback(async (files: FileList | File[]) => {
    const imageFiles = Array.from(files).filter((file) =>
      file.type.startsWith('image/')
    )
    if (imageFiles.length === 0) return

    const next = await Promise.all(
      imageFiles.map(async (file) => {
        const attachment = await fileToPromptAttachment(file)
        const previewUrl = URL.createObjectURL(file)
        return {
          id: `${file.name}-${file.lastModified}-${Math.random()}`,
          filename: attachment.filename,
          previewUrl,
          attachment,
        }
      })
    )
    setPendingImages((current) => [...current, ...next])
  }, [])

  const removePendingImage = useCallback((id: string) => {
    setPendingImages((current) => {
      const target = current.find((item) => item.id === id)
      if (target) URL.revokeObjectURL(target.previewUrl)
      return current.filter((item) => item.id !== id)
    })
  }, [])

  const submit = useCallback(async () => {
    const trimmed = value.trim()
    if (
      (!trimmed && pendingImages.length === 0) ||
      !activeSessionId ||
      !canSubmit
    ) {
      return
    }
    closeSlashTrigger()
    const attachments = pendingImages.map((item) => item.attachment)
    const accepted = await submitPrompt(trimmed, attachments)
    if (!accepted) return
    setValue('')
    setPendingImages((current) => {
      for (const item of current) URL.revokeObjectURL(item.previewUrl)
      return []
    })
    if (textareaRef.current) {
      textareaRef.current.style.height = 'auto'
    }
  }, [
    value,
    pendingImages,
    activeSessionId,
    canSubmit,
    submitPrompt,
    closeSlashTrigger,
  ])

  const handleKeyDown = useCallback(
    (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
      // 当命令面板可见时，放行导航键给 CommandSelector 的全局监听
      if (slashTriggerVisible) {
        switch (event.key) {
          case 'Escape':
            event.preventDefault()
            closeSlashTrigger()
            return
          case 'ArrowUp':
          case 'ArrowDown':
            event.preventDefault()
            return
        }
      }

      if (event.key === 'Enter' && !event.shiftKey && !isComposing) {
        event.preventDefault()
        submit().catch((err) => console.error('submit failed:', err))
      }
    },
    [submit, isComposing, slashTriggerVisible, closeSlashTrigger]
  )

  // Abort 防抖处理：500ms 内只允许一次 abort 调用
  const handleAbort = useCallback(() => {
    if (abortInProgressRef.current) return

    abortInProgressRef.current = true
    abortCurrentTurn().finally(() => {
      // 500ms 后重置，允许再次 abort
      if (abortDebounceRef.current) {
        clearTimeout(abortDebounceRef.current)
      }
      abortDebounceRef.current = window.setTimeout(() => {
        abortInProgressRef.current = false
        abortDebounceRef.current = null
      }, 500)
    })
  }, [abortCurrentTurn])

  return (
    <div className="shrink-0 bg-panel-bg px-(--chat-content-horizontal-padding) pb-4.5 pt-4">
      <div className="mx-auto w-full max-w-(--chat-composer-max-width) translate-x-(--chat-assistant-center-shift)">
        <div className="relative w-full">
          <div className={composerShell}>
            {workingDir && (
              <div
                className="flex items-center gap-2 rounded-t-[23px] border-b border-border bg-white/40 px-4 py-2.5 text-text-secondary"
                title={workingDir}
              >
                <span
                  className="inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center"
                  aria-hidden="true"
                >
                  <svg className="h-3.5 w-3.5" viewBox="0 0 20 20">
                    <path
                      d="M2.5 5.75A1.75 1.75 0 0 1 4.25 4h4.03c.46 0 .9.18 1.23.5l1.02 1c.32.3.74.47 1.18.47h4.04A1.75 1.75 0 0 1 17.5 7.72v6.53A1.75 1.75 0 0 1 15.75 16H4.25A1.75 1.75 0 0 1 2.5 14.25V5.75Z"
                      fill="none"
                      stroke="currentColor"
                      strokeLinejoin="round"
                      strokeWidth="1.4"
                    />
                  </svg>
                </span>
                <div className="overflow-hidden text-ellipsis whitespace-nowrap font-mono text-xs">
                  {workingDir}
                </div>
              </div>
            )}
            <div className="relative">
              <div className="flex flex-col px-(--chat-composer-shell-padding-x) py-3">
                {pendingImages.length > 0 ? (
                  <div className="mb-3 flex flex-wrap gap-2">
                    {pendingImages.map((image) => (
                      <div
                        key={image.id}
                        className="relative overflow-hidden rounded-xl border border-border bg-white/60"
                      >
                        <img
                          src={image.previewUrl}
                          alt={image.filename}
                          className="h-20 w-20 object-cover"
                        />
                        <button
                          type="button"
                          className="absolute right-1 top-1 rounded-full bg-black/55 px-1.5 text-[11px] text-white"
                          onClick={() => removePendingImage(image.id)}
                          aria-label={`移除 ${image.filename}`}
                        >
                          ×
                        </button>
                      </div>
                    ))}
                  </div>
                ) : null}
                <textarea
                  ref={textareaRef}
                  className="mb-3 max-h-60 min-h-12.5 w-full resize-none overflow-y-auto border-0 bg-transparent p-0 text-[15px] leading-[1.75] text-text-primary placeholder:text-text-muted focus:outline-none disabled:cursor-not-allowed disabled:opacity-60"
                  placeholder="向 AstrCode 提问..."
                  value={value}
                  rows={1}
                  onChange={handleInput}
                  onClick={handleCursorActivity}
                  onKeyDown={handleKeyDown}
                  onKeyUp={handleCursorActivity}
                  onPaste={(event) => {
                    const items = event.clipboardData?.items
                    if (!items) return
                    const files: File[] = []
                    for (const item of items) {
                      if (
                        item.kind === 'file' &&
                        item.type.startsWith('image/')
                      ) {
                        const file = item.getAsFile()
                        if (file) files.push(file)
                      }
                    }
                    if (files.length > 0) {
                      event.preventDefault()
                      void addImageFiles(files)
                    }
                  }}
                  onCompositionStart={() => setIsComposing(true)}
                  onCompositionEnd={() => setIsComposing(false)}
                  disabled={!activeSessionId}
                />
                <div className="flex items-center justify-between">
                  <div className="flex shrink-0 items-center gap-2">
                    <input
                      ref={fileInputRef}
                      type="file"
                      accept="image/*"
                      multiple
                      className="hidden"
                      onChange={(event) => {
                        const files = event.target.files
                        if (files) void addImageFiles(files)
                        event.target.value = ''
                      }}
                    />
                    <button
                      type="button"
                      className="rounded-full border border-border px-2.5 py-1 text-[11px] text-text-secondary hover:bg-white/50 disabled:opacity-50"
                      onClick={() => fileInputRef.current?.click()}
                      disabled={!activeSessionId}
                    >
                      图片
                    </button>
                    <ModelSelector
                      refreshKey={modelRefreshKey}
                      getCurrentModel={api.getCurrentModel}
                      listAvailableModels={api.listModels}
                      setModel={async (profileName, model) => {
                        await api.updateActiveSelection(profileName, model)
                        bumpModelRefreshKey()
                      }}
                    />
                    {Object.entries(statusItems)
                      .filter(([, v]) => v)
                      .map(([id, text]) => (
                        <span
                          key={id}
                          className="text-[11px] text-text-secondary"
                        >
                          {text}
                        </span>
                      ))}
                    {queuedMessages.length > 0 && (
                      <span className="text-[11px] text-text-secondary">
                        {queuedMessages.length} 条排队中
                      </span>
                    )}
                  </div>
                  <div className="flex shrink-0 items-center gap-2">
                    {isBusy && (
                      <button
                        className={composerInterruptButton}
                        type="button"
                        onClick={handleAbort}
                        disabled={compactSubmitting}
                      >
                        {compactSubmitting ? (
                          <span className="inline-flex items-center gap-1.5">
                            <span className="h-3 w-3 animate-spin rounded-full border-2 border-current border-t-transparent" />
                            压缩中...
                          </span>
                        ) : (
                          '中断'
                        )}
                      </button>
                    )}
                    <button
                      className={cn(composerSubmitButton)}
                      type="button"
                      onClick={() => void submit()}
                      disabled={!hasInput || !activeSessionId || !canSubmit}
                      aria-label={isBusy ? '加入队列' : '发送消息'}
                      title={isBusy ? '加入队列' : '发送消息'}
                    >
                      <svg
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        strokeWidth="2.5"
                        strokeLinecap="round"
                        strokeLinejoin="round"
                      >
                        <line x1="12" y1="19" x2="12" y2="5"></line>
                        <polyline points="5 12 12 5 19 12"></polyline>
                      </svg>
                    </button>
                  </div>
                </div>
              </div>
            </div>
          </div>
          {activeSessionId && slashTriggerVisible && (
            <CommandSelector
              key={`${activeSessionId}:${slashQuery}`}
              visible={slashTriggerVisible}
              options={slashOptions}
              loading={slashLoading}
              query={slashQuery}
              onSelect={handleSlashCommandSelect}
              onClose={closeSlashTrigger}
            />
          )}
        </div>
      </div>
      <div className="mx-auto mt-2.5 w-full max-w-(--chat-composer-max-width) text-center text-xs text-text-muted">
        AI 可能会产生误导性信息，请核实重要内容
      </div>
    </div>
  )
}
