import type { SVGProps } from 'react'

export type IconName =
  | 'sidebar'
  | 'send'
  | 'close'
  | 'chevron-right'
  | 'plug'
  | 'folder'
  | 'project'
  | 'edit'
  | 'settings'
  | 'users'
  | 'copy'
  | 'retry'
  | 'chevron-down'
  | 'trash'
  | 'plus'
  | 'shield'
  | 'monitor'
  | 'branch'

type IconProps = SVGProps<SVGSVGElement> & {
  name: IconName
  size?: number
}

const icons: Record<
  IconName,
  (props: SVGProps<SVGSVGElement>) => React.ReactElement
> = {
  sidebar: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <rect x="3" y="3" width="18" height="18" rx="2" ry="2" strokeWidth="2" />
      <line x1="9" y1="3" x2="9" y2="21" strokeWidth="2" />
    </svg>
  ),
  send: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <line x1="12" y1="19" x2="12" y2="5" strokeWidth="2.5" />
      <polyline points="5 12 12 5 19 12" strokeWidth="2.5" />
    </svg>
  ),
  close: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <line x1="18" y1="6" x2="6" y2="18" strokeWidth="2" />
      <line x1="6" y1="6" x2="18" y2="18" strokeWidth="2" />
    </svg>
  ),
  'chevron-right': (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <polyline points="9 18 15 12 9 6" strokeWidth="2" />
    </svg>
  ),
  plug: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <path d="M12 22v-5" strokeWidth="2" />
      <path d="M9 8V2" strokeWidth="2" />
      <path d="M15 8V2" strokeWidth="2" />
      <path d="M6 8h12v3a6 6 0 0 1-12 0V8Z" strokeWidth="2" />
    </svg>
  ),
  folder: (props) => (
    <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" {...props}>
      <path
        d="M2.5 5.75A1.75 1.75 0 0 1 4.25 4h4.03c.46 0 .9.18 1.23.5l1.02 1c.32.3.74.47 1.18.47h4.04A1.75 1.75 0 0 1 17.5 7.72v6.53A1.75 1.75 0 0 1 15.75 16H4.25A1.75 1.75 0 0 1 2.5 14.25V5.75Z"
        strokeLinejoin="round"
        strokeWidth="1.4"
      />
    </svg>
  ),
  project: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <rect x="5" y="4" width="14" height="16" rx="2" strokeWidth="2" />
      <path d="M9 8h6" strokeWidth="2" />
      <path d="M9 12h6" strokeWidth="2" />
      <path d="M9 16h4" strokeWidth="2" />
    </svg>
  ),
  edit: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <path
        d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"
        strokeWidth="1.5"
      />
      <path
        d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"
        strokeWidth="1.5"
      />
    </svg>
  ),
  settings: (props) => (
    <svg viewBox="0 0 24 24" fill="currentColor" {...props}>
      <path d="M10.4 2h3.2l.5 2.6c.6.2 1.1.5 1.6.9l2.5-.9 1.6 2.8-2 1.7c.1.3.1.6.1.9s0 .6-.1.9l2 1.7-1.6 2.8-2.5-.9c-.5.4-1 .7-1.6.9l-.5 2.6h-3.2l-.5-2.6c-.6-.2-1.1-.5-1.6-.9l-2.5.9-1.6-2.8 2-1.7c-.1-.3-.1-.6-.1-.9s0-.6.1-.9l-2-1.7 1.6-2.8 2.5.9c.5-.4 1-.7 1.6-.9L10.4 2Zm1.6 6.5A3.5 3.5 0 1 0 12 15.5 3.5 3.5 0 0 0 12 8.5Z" />
    </svg>
  ),
  users: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" strokeWidth="2" />
      <circle cx="9" cy="7" r="4" strokeWidth="2" />
      <path d="M22 21v-2a4 4 0 0 0-3-3.87" strokeWidth="2" />
      <path d="M16 3.13a4 4 0 0 1 0 7.75" strokeWidth="2" />
    </svg>
  ),
  copy: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <rect x="9" y="9" width="13" height="13" rx="2" strokeWidth="2" />
      <path
        d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"
        strokeWidth="2"
      />
    </svg>
  ),
  retry: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <path d="M1 4v6h6" strokeWidth="2" />
      <path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10" strokeWidth="2" />
    </svg>
  ),
  'chevron-down': (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <polyline points="6 9 12 15 18 9" strokeWidth="2" />
    </svg>
  ),
  trash: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <polyline points="3 6 5 6 21 6" strokeWidth="2" />
      <path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" strokeWidth="2" />
      <path d="M10 11v6" strokeWidth="2" />
      <path d="M14 11v6" strokeWidth="2" />
      <path d="M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2" strokeWidth="2" />
    </svg>
  ),
  plus: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <path d="M12 5v14" strokeWidth="2" />
      <path d="M5 12h14" strokeWidth="2" />
    </svg>
  ),
  shield: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <path
        d="M12 3 19 6v5c0 4.5-2.8 8.4-7 10-4.2-1.6-7-5.5-7-10V6l7-3Z"
        strokeWidth="2"
      />
      <path d="m9 12 2 2 4-4" strokeWidth="2" />
    </svg>
  ),
  monitor: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <rect x="3" y="4" width="18" height="13" rx="2" strokeWidth="2" />
      <path d="M8 21h8" strokeWidth="2" />
      <path d="M12 17v4" strokeWidth="2" />
    </svg>
  ),
  branch: (props) => (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" {...props}>
      <circle cx="6" cy="18" r="3" strokeWidth="2" />
      <circle cx="18" cy="6" r="3" strokeWidth="2" />
      <path d="M6 15V5" strokeWidth="2" />
      <path d="M6 5h6a6 6 0 0 1 6 6v-2" strokeWidth="2" />
    </svg>
  ),
}

export function Icon({ name, size = 16, className, ...props }: IconProps) {
  const Component = icons[name]
  return (
    <Component
      width={size}
      height={size}
      className={className}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      {...props}
    />
  )
}
