import {
  useRef,
  useState,
  useCallback,
  useEffect,
  type ClipboardEvent,
} from 'react'
import { useAppStore } from '../../store/conversation'
import {
  composerShell,
  composerSubmitButton,
  composerInterruptButton,
  ghostIconButton,
} from '../../lib/styles'
import { cn } from '../../lib/utils'
import ModelSelector from './ModelSelector'
import CommandSelector from './CommandSelector'
import PendingMessagesPanel from './PendingMessagesPanel'
import ComposerAttachments from './ComposerAttachments'
import {
  attachmentToWire,
  MAX_ATTACHMENTS,
  readImageFiles,
  revokeAttachmentPreviews,
} from '../../lib/composerAttachments'
import type { ConfigView, PromptAttachment } from '../../services/types'
import { Icon } from '../ui'
import * as api from '../../services/api'
import type { SlashCommandInfo } from '../../services/types'
import { canInjectMidTurn, isExecutionPhase } from '../../store/phaseHelpers'

interface InputBarProps {
  presentation?: 'docked' | 'hero'
}

function projectNameFromDir(workingDir: string): string {
  return workingDir.split(/[\\/]/).filter(Boolean).pop() ?? workingDir
}

export default function InputBar({ presentation = 'docked' }: InputBarProps) {
  const submitPrompt = useAppStore((s) => s.submitPrompt)
  const abortCurrentTurn = useAppStore((s) => s.abortCurrentTurn)
  const phase = useAppStore((s) => s.phase)
  const control = useAppStore((s) => s.control)
  const workingDir = useAppStore((s) => s.workingDir)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const modelRefreshKey = useAppStore((s) => s.modelRefreshKey)
  const bumpModelRefreshKey = useAppStore((s) => s.bumpModelRefreshKey)
  const compactSubmitting = useAppStore((s) => s.compactSubmitting)
  const statusItems = useAppStore((s) => s.statusItems)
  const pendingMessages = useAppStore((s) => s.pendingMessages)
  const composerDeliveryMode = useAppStore((s) => s.composerDeliveryMode)
  const toggleComposerDeliveryMode = useAppStore(
    (s) => s.toggleComposerDeliveryMode
  )
  const flushPendingQueued = useAppStore((s) => s.flushPendingQueued)

  const [value, setValue] = useState('')
  const [attachments, setAttachments] = useState<PromptAttachment[]>([])
  const [configView, setConfigView] = useState<ConfigView | null>(null)
  const [approvalSaving, setApprovalSaving] = useState(false)
  const attachmentsRef = useRef(attachments)
  useEffect(() => {
    attachmentsRef.current = attachments
  }, [attachments])
  const [isComposing, setIsComposing] = useState(false)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)
  const isHero = presentation === 'hero'
  const isCompacting = phase === 'compacting' || compactSubmitting
  const isBusy = isExecutionPhase(phase, compactSubmitting)
  const canSubmit = !!activeSessionId && !isCompacting
  const canInject = canInjectMidTurn(control, compactSubmitting)
  const submitActionLabel = isBusy
    ? composerDeliveryMode === 'queued'
      ? '加入队列'
      : '注入当前 turn'
    : '发送消息'

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

  useEffect(() => {
    return () => revokeAttachmentPreviews(attachmentsRef.current)
  }, [])

  useEffect(() => {
    let cancelled = false
    api
      .getConfig()
      .then((config) => {
        if (!cancelled) setConfigView(config)
      })
      .catch(() => {
        if (!cancelled) setConfigView(null)
      })
    return () => {
      cancelled = true
    }
  }, [modelRefreshKey])

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

  const addAttachments = useCallback((incoming: PromptAttachment[]) => {
    if (incoming.length === 0) return
    setAttachments((current) => {
      const merged = [...current, ...incoming]
      if (merged.length <= MAX_ATTACHMENTS) return merged
      revokeAttachmentPreviews(merged.slice(MAX_ATTACHMENTS))
      return merged.slice(0, MAX_ATTACHMENTS)
    })
  }, [])

  const removeAttachment = useCallback((id: string) => {
    setAttachments((current) => {
      const target = current.find((item) => item.id === id)
      if (target) URL.revokeObjectURL(target.previewUrl)
      return current.filter((item) => item.id !== id)
    })
  }, [])

  const handleAttachFromPicker = useCallback(
    (event: React.ChangeEvent<HTMLInputElement>) => {
      const files = Array.from(event.target.files ?? [])
      addAttachments(readImageFiles(files))
      event.target.value = ''
    },
    [addAttachments]
  )

  const toggleApprovalMode = useCallback(async () => {
    if (!configView || approvalSaving) return

    const nextApprovalMode =
      configView.approvalMode === 'yolo' ? 'manual' : 'yolo'
    setApprovalSaving(true)
    try {
      await api.updateActiveSelection(
        configView.activeProfile,
        configView.activeModel,
        configView.activeSmallProfile,
        configView.activeSmallModel,
        nextApprovalMode
      )
      setConfigView({ ...configView, approvalMode: nextApprovalMode })
      bumpModelRefreshKey()
    } catch (err) {
      console.error('update approval mode failed:', err)
    } finally {
      setApprovalSaving(false)
    }
  }, [approvalSaving, bumpModelRefreshKey, configView])

  const handlePaste = useCallback(
    (event: ClipboardEvent<HTMLTextAreaElement>) => {
      const items = event.clipboardData?.items
      if (!items) return
      const files: File[] = []
      for (const item of items) {
        if (item.kind !== 'file') continue
        const file = item.getAsFile()
        if (file?.type.startsWith('image/')) files.push(file)
      }
      if (files.length === 0) return
      event.preventDefault()
      addAttachments(readImageFiles(files))
    },
    [addAttachments]
  )

  const submit = useCallback(async () => {
    const trimmed = value.trim()
    if (
      (!trimmed && attachments.length === 0) ||
      !activeSessionId ||
      !canSubmit
    ) {
      return
    }
    closeSlashTrigger()
    const wireAttachments = await Promise.all(attachments.map(attachmentToWire))
    const accepted = await submitPrompt(trimmed, wireAttachments)
    if (!accepted) return
    revokeAttachmentPreviews(attachments)
    setAttachments([])
    setValue('')
    if (textareaRef.current) {
      textareaRef.current.style.height = 'auto'
    }
  }, [
    value,
    attachments,
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

  useEffect(() => {
    if (isBusy) return
    void flushPendingQueued()
  }, [isBusy, flushPendingQueued, pendingMessages.length])

  const projectName = workingDir ? projectNameFromDir(workingDir) : null
  const approvalMode = configView?.approvalMode ?? null
  const approvalLabel =
    approvalMode === 'yolo'
      ? '完全访问'
      : approvalMode === 'manual'
        ? '手动确认'
        : '权限模式'
  const branchLabel =
    statusItems['git-branch'] ?? statusItems.branch ?? statusItems.gitBranch
  const extraStatusItems = Object.entries(statusItems).filter(
    ([id, text]) => text && !['git-branch', 'branch', 'gitBranch'].includes(id)
  )

  return (
    <div
      className={cn(
        'shrink-0',
        isHero
          ? 'w-full'
          : 'bg-panel-bg px-[var(--layout-page-padding-x)] pb-5 pt-2'
      )}
    >
      <div
        className={cn(
          'w-full translate-x-[var(--chat-assistant-center-shift)]',
          'mx-auto max-w-[var(--layout-content-max-width)]'
        )}
      >
        <PendingMessagesPanel
          canInject={canInject}
          onEdit={(text) => {
            setValue(text)
            requestAnimationFrame(() => {
              const textarea = textareaRef.current
              if (!textarea) return
              textarea.focus()
              textarea.style.height = 'auto'
              textarea.style.height = `${Math.min(textarea.scrollHeight, 200)}px`
            })
          }}
        />
        <div className="relative w-full">
          <div className={composerShell}>
            <div
              className={cn(
                'relative',
                isHero
                  ? 'px-[var(--layout-content-inset-x)] pb-3 pt-4'
                  : 'px-[var(--layout-content-inset-x)] pb-3.5 pt-4 sm:pt-5'
              )}
            >
              <ComposerAttachments
                attachments={attachments}
                onRemove={removeAttachment}
              />
              <textarea
                ref={textareaRef}
                className={cn(
                  'w-full resize-none overflow-y-auto border-0 bg-transparent p-0 text-text-primary placeholder:text-text-muted focus:outline-none disabled:cursor-not-allowed disabled:opacity-60',
                  isHero
                    ? 'mb-5 max-h-44 min-h-12 text-[16px] leading-[1.55]'
                    : 'mb-5 max-h-60 min-h-14 text-[16px] leading-[1.6]'
                )}
                placeholder={isHero ? '随心输入' : '向 AstrCode 提问...'}
                value={value}
                rows={1}
                onChange={handleInput}
                onClick={handleCursorActivity}
                onKeyDown={handleKeyDown}
                onKeyUp={handleCursorActivity}
                onCompositionStart={() => setIsComposing(true)}
                onCompositionEnd={() => setIsComposing(false)}
                onPaste={handlePaste}
                disabled={!activeSessionId}
              />
              <input
                ref={fileInputRef}
                type="file"
                accept="image/*"
                multiple
                className="hidden"
                onChange={handleAttachFromPicker}
              />
              <div className="flex min-h-10 items-center justify-between gap-3">
                <div className="flex min-w-0 shrink items-center gap-2.5">
                  <button
                    type="button"
                    className="inline-flex h-10 w-10 shrink-0 items-center justify-center rounded-full text-text-muted transition-colors hover:bg-surface-muted hover:text-text-primary"
                    onClick={() => fileInputRef.current?.click()}
                    aria-label="添加图片"
                    title="添加图片"
                    disabled={!activeSessionId}
                  >
                    <Icon name="plus" size={22} />
                  </button>
                  <button
                    type="button"
                    className={cn(
                      'inline-flex h-9 shrink-0 items-center gap-1.5 rounded-full px-2.5 text-[14px] font-semibold transition-colors hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-60',
                      approvalMode === 'yolo'
                        ? 'text-accent'
                        : 'text-text-secondary'
                    )}
                    onClick={() => void toggleApprovalMode()}
                    disabled={!configView || approvalSaving}
                    aria-label="切换工具权限模式"
                    title={
                      approvalMode === 'yolo'
                        ? '当前为 YOLO / 完全访问，点击切换为手动确认'
                        : '当前为手动确认，点击切换为 YOLO / 完全访问'
                    }
                  >
                    <Icon name="shield" size={15} />
                    {approvalLabel}
                  </button>
                  {projectName && (
                    <div
                      className="hidden min-w-0 max-w-[180px] items-center gap-1.5 rounded-full px-2 text-[13px] text-text-muted lg:flex"
                      title={workingDir ?? undefined}
                    >
                      <Icon name="project" size={15} className="shrink-0" />
                      <span className="truncate font-medium">
                        {projectName}
                      </span>
                    </div>
                  )}
                  {branchLabel && (
                    <div className="hidden min-w-0 max-w-[160px] items-center gap-1.5 rounded-full px-2 text-[13px] text-text-muted xl:flex">
                      <Icon name="branch" size={15} className="shrink-0" />
                      <span className="truncate">{branchLabel}</span>
                    </div>
                  )}
                  {extraStatusItems.map(([id, text]) => (
                    <span
                      key={id}
                      className="hidden min-w-0 max-w-[140px] truncate rounded-full px-2 text-[13px] text-text-muted 2xl:inline-flex"
                    >
                      {text}
                    </span>
                  ))}
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                  <ModelSelector
                    refreshKey={modelRefreshKey}
                    getCurrentModel={api.getCurrentModel}
                    listAvailableModels={api.listModels}
                    setModel={async (profileName, model) => {
                      await api.updateActiveSelection(
                        profileName,
                        model,
                        configView?.activeSmallProfile,
                        configView?.activeSmallModel,
                        configView?.approvalMode ?? 'manual'
                      )
                      bumpModelRefreshKey()
                    }}
                  />
                  {isBusy && (
                    <button
                      type="button"
                      className={cn(
                        ghostIconButton,
                        'gap-1 px-2 py-1.5 text-[11px]',
                        composerDeliveryMode === 'inject' && 'text-accent',
                        composerDeliveryMode === 'inject' &&
                          !canInject &&
                          'opacity-50'
                      )}
                      onClick={toggleComposerDeliveryMode}
                      aria-label={
                        composerDeliveryMode === 'queued'
                          ? '切换为 inject'
                          : '切换为 queue'
                      }
                      title={
                        composerDeliveryMode === 'queued'
                          ? '下一条：Queue（默认）'
                          : canInject
                            ? '下一条：Inject 到当前 turn'
                            : 'Inject 需要 Agent 正在运行'
                      }
                    >
                      <Icon name="send" size={13} />
                      <span className="font-medium">
                        {composerDeliveryMode === 'queued' ? 'Queue' : 'Inject'}
                      </span>
                    </button>
                  )}
                  {isBusy && (
                    <button
                      className={composerInterruptButton}
                      type="button"
                      onClick={handleAbort}
                      disabled={isCompacting}
                    >
                      {isCompacting ? (
                        <span className="inline-flex items-center gap-1.5">
                          <span className="h-3 w-3 animate-spin rounded-full border-2 border-current border-t-transparent" />
                          压缩中
                        </span>
                      ) : (
                        'Stop'
                      )}
                    </button>
                  )}
                  <button
                    className={cn(composerSubmitButton)}
                    type="button"
                    onClick={() => void submit()}
                    disabled={
                      (!value.trim() && attachments.length === 0) ||
                      !activeSessionId ||
                      !canSubmit
                    }
                    aria-label={submitActionLabel}
                    title={submitActionLabel}
                  >
                    <Icon name="send" size={14} />
                  </button>
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
      {!isHero && (
        <p className="mx-auto mt-2 w-full max-w-[var(--layout-content-max-width)] text-center text-[11px] text-text-muted">
          AI 可能会产生误导性信息，请核实重要内容
        </p>
      )}
    </div>
  )
}
