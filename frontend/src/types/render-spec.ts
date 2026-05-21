/**
 * RenderSpec — structured UI rendering protocol.
 *
 * Mirrors `astrcode-core::render::RenderSpec` (Rust).
 * Serialized via `#[serde(tag = "type", rename_all = "snake_case")]`,
 * so each variant has a `"type"` discriminator field.
 *
 * Tools place a `RenderSpec` in `ToolResult.metadata["ui_render"]`.
 * The frontend reads this key to decide how to render the tool result,
 * instead of hardcoding per-tool display logic.
 */

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

// ── Helpers ───────────────────────────────────────────────────────────────

const RENDER_SPEC_TYPES = new Set([
  'text',
  'markdown',
  'box',
  'list',
  'key_value',
  'progress',
  'diff',
  'code',
  'image_ref',
  'raw_ansi_limited',
])

/** Metadata key where tools embed RenderSpec. */
export const UI_RENDER_METADATA_KEY = 'ui_render'

/** Type guard: check if a value looks like a RenderSpec. */
export function isRenderSpec(value: unknown): value is RenderSpec {
  if (typeof value !== 'object' || value === null) return false
  const obj = value as Record<string, unknown>
  return typeof obj.type === 'string' && RENDER_SPEC_TYPES.has(obj.type)
}

/**
 * Extract RenderSpec from tool metadata.
 * Returns `undefined` if not present or malformed.
 */
export function extractRenderSpec(
  metadata?: Record<string, unknown>
): RenderSpec | undefined {
  if (!metadata) return undefined
  const raw = metadata[UI_RENDER_METADATA_KEY]
  return isRenderSpec(raw) ? raw : undefined
}

/**
 * Plain-text fallback for RenderSpec (mirrors Rust `plain_text_fallback()`).
 * Useful for accessibility, copy-to-clipboard, or when rich rendering is unavailable.
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
