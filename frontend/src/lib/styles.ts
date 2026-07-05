// Shared style constants
export const pillBase =
  'inline-flex min-h-[20px] shrink-0 items-center rounded px-1.5 text-[11px] font-medium tracking-normal'
export const pillNeutral = `${pillBase} text-text-muted`
export const pillSuccess = `${pillBase} text-success`
export const pillDanger = `${pillBase} text-danger`

export const ghostIconButton =
  'inline-flex items-center justify-center rounded-lg text-text-muted transition-colors duration-150 hover:bg-surface-muted hover:text-text-primary'
export const chevronIcon =
  'inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center text-text-muted transition-transform duration-150 ease-out group-open:rotate-90'

export const composerShell =
  'rounded-[22px] border border-border bg-surface shadow-composer-shell transition-[border-color,box-shadow] duration-150 focus-within:border-border-strong focus-within:shadow-focus-accent'
export const composerSubmitButton =
  'inline-flex h-10 w-10 flex-shrink-0 items-center justify-center rounded-full bg-btn-primary-bg text-btn-primary-fg transition-[opacity,transform] duration-150 hover:opacity-90 active:scale-[0.97] disabled:cursor-not-allowed disabled:opacity-30 [&_svg]:h-4 [&_svg]:w-4'
export const composerInterruptButton =
  'h-9 flex-shrink-0 rounded-full px-3 text-[12px] font-medium text-text-secondary transition-colors duration-150 hover:bg-surface-muted hover:text-text-primary'

export const codeBlockShell =
  'group relative my-2 overflow-hidden rounded-lg border border-code-border bg-code-surface'
export const codeBlockHeader =
  'flex items-center justify-between bg-code-surface px-4 pb-1 pt-2 text-xs text-code-label'
export const codeBlockContent =
  'm-0 overflow-x-auto whitespace-pre px-4 pb-4 pt-2 font-mono text-sm leading-relaxed text-code-text'

export const errorSurface =
  'self-stretch rounded-lg border border-danger/15 bg-danger-soft/50 px-4 py-3 text-danger'
export const emptyStateSurface = 'px-6 py-8 text-center text-sm text-text-muted'
export const assistantAvatar =
  'inline-flex h-[48px] w-[48px] shrink-0 items-center justify-center rounded-full bg-transparent text-accent'
export const expandableBody = 'mb-3 ml-2 mt-2 border-l-2 border-border pl-4'

/** ToolCallBlock 内容区内边距（与 toolCodePreviewBleed 配套）。 */
export const toolPanelPaddingX = 'px-4'
/** 代码预览向卡片内缘延伸，抵消外层 padding，左侧略紧以贴近行号。 */
export const toolCodePreviewBleed = '-mx-4 pl-3 pr-4'
export const toolPanelScrollViewport =
  'min-w-0 max-h-[min(58vh,560px)] overflow-auto overscroll-contain'

// Dialog
export const overlay =
  'fixed inset-0 z-[10000] flex items-center justify-center bg-overlay-backdrop p-5 backdrop-blur-[8px]'
export const dialogSurface =
  'rounded-xl border border-border bg-surface p-6 shadow-surface-lg w-[460px] max-w-[calc(100vw-32px)]'
export const fieldInput =
  'w-full rounded-lg border border-border bg-surface px-3 py-2.5 text-[13px] text-text-primary outline-none transition-[border-color,box-shadow] duration-150 placeholder:text-text-muted focus:border-border-strong focus:shadow-focus-accent'
export const fieldButton =
  'flex w-full items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-2.5 text-[13px] text-text-primary transition-colors duration-150 hover:bg-surface-muted focus-visible:border-border-strong focus-visible:outline-none disabled:cursor-not-allowed disabled:opacity-55'
export const btnSecondary =
  'rounded-lg border border-border bg-surface px-4 py-2 text-[13px] font-medium text-text-secondary transition-colors duration-150 hover:bg-surface-muted hover:text-text-primary'
export const btnPrimary =
  'rounded-lg border-none bg-btn-primary-bg px-4 py-2 text-[13px] font-medium text-btn-primary-fg transition-opacity duration-150 hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-40'
export const overlayBackdrop = 'var(--overlay-backdrop)'

export const PHASE_BG_CLASS: Record<string, string> = {
  idle: 'bg-phase-idle',
  thinking: 'bg-phase-thinking',
  calling_tool: 'bg-phase-calling-tool',
  streaming: 'bg-phase-streaming',
  compacting: 'bg-phase-thinking',
  error: 'bg-phase-error',
}
