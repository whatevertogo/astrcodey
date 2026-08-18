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
import ArgumentCompletionSelector from './ArgumentCompletionSelector'
import PendingMessagesPanel from './PendingMessagesPanel'
import ComposerAttachments from './ComposerAttachments'
import {
  attachmentToWire,
  MAX_ATTACHMENTS,
  readImageFiles,
  revokeAttachmentPreviews,
} from '../../lib/composerAttachments'
import type {
  CommandCompletionItem,
  ConfigView,
  PromptAttachment,
  SlashCommandInfo,
} from '../../services/types'
import { Icon } from '../ui'
import * as api from '../../services/api'
import {
  canInjectMidTurn,
  effectiveConversationPhase,
  isExecutionPhase,
} from '../../store/phaseHelpers'

interface InputBarProps {
  presentation?: 'docked' | 'hero'
}

function projectNameFromDir(workingDir: string): string {
  return workingDir.split(/[\\/]/).filter(Boolean).pop() ?? workingDir
}

export default function InputBar({ presentation = 'docked' }: InputBarProps) {
  const submitPrompt = useAppStore((s) => s.submitPrompt)
  const abortCurrentTurn = useAppStore((s) => s.abortCurrentTurn)
  const control = useAppStore((s) => s.control)
  const workingDir = useAppStore((s) => s.workingDir)
  const activeSessionId = useAppStore((s) => s.activeSessionId)
  const modelRefreshKey = useAppStore((s) => s.modelRefreshKey)
  const bumpModelRefreshKey = useAppStore((s) => s.bumpModelRefreshKey)
  const compactSubmitting = useAppStore((s) => s.compactSubmitting)
  const phase = useAppStore((state) =>
    effectiveConversationPhase(state.control, state.compactSubmitting)
  )
  const statusItems = useAppStore((s) => s.statusItems)
  const slashCommands = useAppStore((s) => s.slashCommands)
  const refreshCommands = useAppStore((s) => s.refreshCommands)
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
  const isCompacting = phase === 'compacting'
  const isBusy = isExecutionPhase(phase)
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
  const [slashLoading, setSlashLoading] = useState(false)
  const slashTriggerStartRef = useRef(0)
  const slashTriggerEndRef = useRef(0)

  // ── argument completion state ──
  const [argTrigger, setArgTrigger] = useState<{
    commandName: string
    argumentStart: number
    cursorPos: number
  } | null>(null)
  const [argItems, setArgItems] = useState<CommandCompletionItem[]>([])
  const [argTruncated, setArgTruncated] = useState(false)
  const [argLoading, setArgLoading] = useState(false)
  const argSeqRef = useRef(0)

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
    setSlashLoading(false)
  }, [])

  const closeArgTrigger = useCallback(() => {
    argSeqRef.current += 1
    setArgTrigger(null)
    setArgItems([])
    setArgTruncated(false)
    setArgLoading(false)
  }, [])

  const updateArgTrigger = useCallback(
    (currentValue: string, cursorPos: number) => {
      // 在当前行找到光标前的 `/name args` 参数补全上下文。
      const lineStart = Math.max(
        0,
        currentValue.lastIndexOf('\n', cursorPos - 1) + 1
      )
      const segment = currentValue.slice(lineStart, cursorPos)
      const match = /^\/(\S+)\s+/.exec(segment)
      const command = match
        ? slashCommands.find(
            (c) => c.name.toLowerCase() === match[1].toLowerCase()
          )
        : undefined
      const trigger =
        match && command?.argumentCompletions
          ? {
              commandName: command.name,
              argumentStart: lineStart + match[0].length,
              cursorPos,
            }
          : null
      if (
        trigger &&
        argTrigger &&
        trigger.commandName === argTrigger.commandName &&
        trigger.argumentStart === argTrigger.argumentStart &&
        trigger.cursorPos === argTrigger.cursorPos
      ) {
        return
      }
      if (!trigger) {
        if (argTrigger) closeArgTrigger()
        return
      }
      setArgTrigger(trigger)
    },
    [slashCommands, argTrigger, closeArgTrigger]
  )

  // 防抖拉取参数补全候选；序号守卫丢弃过期响应。
  useEffect(() => {
    if (!argTrigger || !activeSessionId) return
    const seq = ++argSeqRef.current
    const timer = window.setTimeout(() => {
      setArgLoading(true)
      const argument = value.slice(
        argTrigger.argumentStart,
        argTrigger.cursorPos
      )
      const cursorOffset = argTrigger.cursorPos - argTrigger.argumentStart
      api
        .completeExtensionCommand(
          activeSessionId,
          argTrigger.commandName,
          argument,
          cursorOffset
        )
        .then((response) => {
          if (argSeqRef.current !== seq) return
          setArgItems(response.items)
          setArgTruncated(response.truncated)
        })
        .catch(() => {
          if (argSeqRef.current !== seq) return
          setArgItems([])
        })
        .finally(() => {
          if (argSeqRef.current === seq) setArgLoading(false)
        })
    }, 250)
    return () => window.clearTimeout(timer)
  }, [activeSessionId, argTrigger, value])

  const handleArgSelect = useCallback(
    (item: CommandCompletionItem) => {
      const trigger = argTrigger
      if (!trigger) return
      const before = value.slice(0, trigger.argumentStart)
      const after = value.slice(trigger.cursorPos)
      const nextValue = `${before}${item.insertText}${after}`
      const nextCursor = trigger.argumentStart + item.insertText.length
      setValue(nextValue)
      closeArgTrigger()

      requestAnimationFrame(() => {
        const textarea = textareaRef.current
        if (!textarea) return
        textarea.focus()
        textarea.setSelectionRange(nextCursor, nextCursor)
        textarea.style.height = 'auto'
        textarea.style.height = `${Math.min(textarea.scrollHeight, 200)}px`
      })
    },
    [argTrigger, value, closeArgTrigger]
  )

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
        setArgTrigger(null)
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
      updateArgTrigger(currentValue, cursorPos)
    },
    [activeSessionId, slashTriggerVisible, closeSlashTrigger, updateArgTrigger]
  )

  // ── fetch commands when panel opens ──
  useEffect(() => {
    if (!slashTriggerVisible || !activeSessionId) return

    let active = true
    void refreshCommands().finally(() => {
      if (active) setSlashLoading(false)
    })

    return () => {
      active = false
    }
  }, [activeSessionId, refreshCommands, slashTriggerVisible])

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
        // 选中命令后立即检测参数补全上下文（如 /goal、/reviewnow）。
        updateArgTrigger(nextValue, nextCursor)
      })
    },
    [closeSlashTrigger, value, updateArgTrigger]
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
        configView.activeSmallProfile ?? undefined,
        configView.activeSmallModel ?? undefined,
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
    closeArgTrigger()
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
    closeArgTrigger,
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

      // 参数补全面板可见时：Escape 关闭，导航键交给面板；Tab/Enter 由
      // ArgumentCompletionSelector 的 capture 监听插入候选。
      if (argTrigger) {
        if (event.key === 'Escape') {
          event.preventDefault()
          closeArgTrigger()
          return
        }
        if (argItems.length > 0 && (event.key === 'ArrowUp' || event.key === 'ArrowDown')) {
          event.preventDefault()
          return
        }
      }

      if (
        event.key === 'Enter' &&
        !event.shiftKey &&
        !isComposing &&
        // WebKit(macOS Tauri = WKWebView)会在 compositionend 之后补发一次
        // keydown(Enter, keyCode 229)，此时 React 状态已复位，必须靠原生标志/键码拦截
        !event.nativeEvent.isComposing &&
        event.keyCode !== 229
      ) {
        event.preventDefault()
        submit().catch((err) => console.error('submit failed:', err))
      }
    },
    [submit, isComposing, slashTriggerVisible, closeSlashTrigger, argTrigger, argItems.length, closeArgTrigger]
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
        ? '请求批准'
        : '权限模式'
  const branchLabel =
    statusItems['git-branch'] ?? statusItems.branch ?? statusItems.gitBranch
  const extraStatusItems = Object.entries(statusItems).filter(
    ([id, text]) => text && !['git-branch', 'branch', 'gitBranch'].includes(id)
  )
  const retryStatus = control?.retryStatus

  return (
    <div
      className={cn(
        'shrink-0',
        isHero
          ? 'w-full'
          : 'bg-gradient-to-t from-panel-bg via-panel-bg to-panel-bg/0 px-[var(--layout-page-padding-x)] pb-4 pt-3'
      )}
    >
      <div
        className={cn(
          'w-full translate-x-[var(--chat-assistant-center-shift)]',
          'mx-auto',
          isHero
            ? 'max-w-[var(--layout-hero-composer-max-width)]'
            : 'max-w-[var(--layout-content-max-width)]'
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
          <div className="mx-4 flex min-h-12 items-start gap-5 overflow-hidden rounded-t-[22px] bg-surface-muted/75 px-6 pb-3 pt-3 text-[13px] text-text-secondary">
            {projectName && (
              <div
                className="flex min-w-0 max-w-[220px] items-center gap-2"
                title={workingDir ?? undefined}
              >
                <Icon name="folder" size={16} className="shrink-0" />
                <span className="truncate font-medium">{projectName}</span>
              </div>
            )}
            <div className="flex shrink-0 items-center gap-2">
              <Icon name="monitor" size={16} />
              <span>本地</span>
            </div>
            {branchLabel && (
              <div className="flex min-w-0 max-w-[180px] items-center gap-2">
                <Icon name="branch" size={16} className="shrink-0" />
                <span className="truncate">{branchLabel}</span>
              </div>
            )}
            {retryStatus && (
              <div
                className="flex shrink-0 items-center gap-2 text-warning"
                role="status"
                aria-live="polite"
              >
                <Icon name="retry" size={16} />
                <span>
                  {retryStatus.status == null
                    ? '连接中断'
                    : `远端 ${retryStatus.status}`}{' '}
                  · 重试 {retryStatus.attempt}/{retryStatus.maxRetries} · 退避{' '}
                  {(retryStatus.delayMs / 1000).toFixed(1)} 秒
                </span>
              </div>
            )}
            {extraStatusItems.map(([id, text]) => (
              <span
                key={id}
                className="hidden min-w-0 max-w-[160px] truncate xl:inline"
              >
                {text}
              </span>
            ))}
          </div>
          <div className={cn(composerShell, 'relative z-10 -mt-2')}>
            <div
              className={cn(
                'relative',
                isHero
                  ? 'px-[var(--layout-content-inset-x)] pb-3 pt-4'
                  : 'px-4 pb-3 pt-3.5'
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
                    : 'mb-3 max-h-60 min-h-10 text-[15px] leading-[1.6]'
                )}
                placeholder={isHero ? '输入任务或问题…' : '向 AstrCode 提问…'}
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
                      'inline-flex h-9 shrink-0 items-center gap-1.5 rounded-full px-2.5 text-[13px] font-medium transition-colors hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-60',
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
                        configView?.activeSmallProfile ?? undefined,
                        configView?.activeSmallModel ?? undefined,
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
              options={slashCommands}
              loading={slashLoading}
              query={slashQuery}
              onSelect={handleSlashCommandSelect}
              onClose={closeSlashTrigger}
            />
          )}
          {activeSessionId && argTrigger && (
            <ArgumentCompletionSelector
              visible={true}
              items={argItems}
              loading={argLoading}
              truncated={argTruncated}
              onSelect={handleArgSelect}
              onClose={closeArgTrigger}
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
