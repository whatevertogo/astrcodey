/** Frontend-local render tree used by built-in tool renderers. */

// ── RenderTone ────────────────────────────────────────────────────────────

export type RenderTone =
  | 'default'
  | 'muted'
  | 'accent'
  | 'success'
  | 'warning'
  | 'error'

// ── RenderKeyValue ────────────────────────────────────────────────────────

export interface RenderKeyValue {
  key: string
  value: string
  tone?: RenderTone
}

// ── RenderSpec ────────────────────────────────────────────────────────────

export type RenderSpec =
  | RenderSpecText
  | RenderSpecMarkdown
  | RenderSpecBox
  | RenderSpecList
  | RenderSpecKeyValue
  | RenderSpecProgress
  | RenderSpecDiff
  | RenderSpecCode
  | RenderSpecImageRef
  | RenderSpecRawAnsi

export interface RenderSpecText {
  type: 'text'
  text: string
  tone?: RenderTone
}

export interface RenderSpecMarkdown {
  type: 'markdown'
  text: string
  tone?: RenderTone
}

export interface RenderSpecBox {
  type: 'box'
  title?: string
  tone?: RenderTone
  children?: RenderSpec[]
}

export interface RenderSpecList {
  type: 'list'
  ordered?: boolean
  items?: RenderSpec[]
  tone?: RenderTone
}

export interface RenderSpecKeyValue {
  type: 'key_value'
  entries?: RenderKeyValue[]
  tone?: RenderTone
}

export interface RenderSpecProgress {
  type: 'progress'
  label: string
  status?: string
  value?: number
  tone?: RenderTone
}

export interface RenderSpecDiff {
  type: 'diff'
  text: string
  tone?: RenderTone
}

export interface RenderSpecCode {
  type: 'code'
  language?: string
  text: string
  tone?: RenderTone
}

export interface RenderSpecImageRef {
  type: 'image_ref'
  uri: string
  alt?: string
  tone?: RenderTone
}

export interface RenderSpecRawAnsi {
  type: 'raw_ansi_limited'
  text: string
  tone?: RenderTone
}

/**
 * Plain-text fallback for accessibility, copy-to-clipboard, or clients without
 * rich rendering.
 */
export function renderSpecToPlainText(spec: RenderSpec): string {
  switch (spec.type) {
    case 'text':
    case 'markdown':
    case 'diff':
    case 'code':
    case 'raw_ansi_limited':
      return spec.text
    case 'box': {
      const parts: string[] = []
      if (spec.title) parts.push(spec.title)
      if (spec.children) {
        for (const child of spec.children)
          parts.push(renderSpecToPlainText(child))
      }
      return parts.join('\n')
    }
    case 'list':
      return (spec.items ?? []).map(renderSpecToPlainText).join('\n')
    case 'key_value':
      return (spec.entries ?? []).map((e) => `${e.key}: ${e.value}`).join('\n')
    case 'progress': {
      let text = spec.label
      if (spec.status) text += ` · ${spec.status}`
      if (spec.value != null) text += ` · ${Math.round(spec.value * 100)}%`
      return text
    }
    case 'image_ref':
      return `[image: ${spec.alt ?? spec.uri}]`
  }
}
