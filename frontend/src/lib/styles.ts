// Shared style constants
export const pillBase = 'inline-flex min-h-[22px] shrink-0 items-center rounded-full px-2.5 text-[11px] font-bold tracking-[0.02em]';
export const pillNeutral = `${pillBase} bg-surface-muted text-text-secondary`;
export const pillSuccess = `${pillBase} bg-success-soft text-success`;
export const pillDanger = `${pillBase} bg-danger-soft text-danger`;

export const ghostIconButton = 'inline-flex items-center justify-center rounded-lg text-text-secondary transition-[background-color,color,opacity] duration-150 ease-out hover:bg-black/5 hover:text-text-primary';
export const chevronIcon = 'inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center text-text-secondary opacity-60 transition-transform duration-150 ease-out group-open:rotate-90';

export const composerShell = 'rounded-[24px] border border-border bg-gradient-to-b from-white/95 to-surface-soft shadow-composer-shell transition-[border-color,box-shadow,transform] duration-[180ms] ease-out focus-within:-translate-y-px focus-within:border-accent-soft/60 focus-within:shadow-focus-accent';
export const composerSubmitButton = 'inline-flex h-9 w-9 flex-shrink-0 items-center justify-center rounded-full bg-accent-strong text-white shadow-soft transition-[transform,filter,opacity] duration-150 ease-out hover:-translate-y-px hover:scale-105 hover:brightness-95 disabled:cursor-not-allowed disabled:opacity-35 [&_svg]:h-4 [&_svg]:w-4';
export const composerInterruptButton = 'h-9 flex-shrink-0 rounded-xl border border-danger bg-danger-soft px-3.5 text-[13px] font-semibold text-danger transition-[filter,opacity] duration-150 ease-out hover:brightness-98';

export const codeBlockShell = 'group relative my-4 overflow-hidden rounded-lg border border-code-border bg-code-surface';
export const codeBlockHeader = 'flex items-center justify-between bg-code-surface px-4 pb-1 pt-2 text-xs text-code-label';
export const codeBlockContent = 'm-0 overflow-x-auto px-4 pb-4 pt-2 font-mono text-sm leading-relaxed text-code-text';

export const errorSurface = 'self-stretch rounded-2xl border border-danger/20 bg-danger-soft px-4 py-3.5 text-danger';
export const emptyStateSurface = 'rounded-[18px] border border-dashed border-border bg-surface/60 px-7 py-6 text-center text-sm text-text-secondary';
export const assistantAvatar = 'inline-flex h-7 w-7 shrink-0 items-center justify-center rounded bg-linear-to-b from-avatar-surface to-avatar-surface-strong text-avatar-text';
export const expandableBody = 'mb-3 ml-2 mt-2 border-l-2 border-border pl-4';

// Dialog
export const overlay = 'fixed inset-0 z-[10000] flex items-center justify-center bg-overlay-backdrop p-5 backdrop-blur-[8px]';
export const dialogSurface = 'rounded-[20px] border border-border bg-surface p-6 shadow-surface-lg';
export const fieldInput = 'w-full rounded-xl border border-border bg-surface px-3 py-[11px] text-[13px] text-text-primary outline-none transition-[border-color,box-shadow,background-color] duration-150 ease-out placeholder:text-text-muted focus:border-border-strong focus:shadow-focus-warm';
export const fieldButton = 'flex w-full items-center justify-between gap-3 rounded-xl border border-border bg-surface px-3 py-[11px] text-[13px] text-text-primary transition-[border-color,background-color,box-shadow] duration-150 ease-out hover:bg-white focus-visible:border-border-strong focus-visible:outline-none disabled:cursor-not-allowed disabled:opacity-55';
export const btnSecondary = 'rounded-xl border border-border bg-surface-soft px-4 py-2.5 text-[13px] font-semibold text-text-secondary transition-[background-color,border-color,color] duration-150 ease-out hover:border-border-strong hover:bg-white hover:text-text-primary';
export const btnPrimary = 'rounded-xl border-none bg-accent-strong px-4 py-2.5 text-[13px] font-semibold text-white transition-[filter,opacity] duration-150 ease-out hover:brightness-95 disabled:cursor-not-allowed disabled:opacity-40';
export const overlayBackdrop = 'rgba(55, 42, 26, 0.18)';

export const PHASE_BG_CLASS: Record<string, string> = {
  idle: 'bg-phase-idle',
  thinking: 'bg-phase-thinking',
  calling_tool: 'bg-phase-calling-tool',
  streaming: 'bg-phase-streaming',
  compacting: 'bg-phase-thinking',
  error: 'bg-phase-error',
};
