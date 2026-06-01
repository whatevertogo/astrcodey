import type { PromptAttachment } from '../services/types'

export const MAX_ATTACHMENTS = 4
export const MAX_ATTACHMENT_CONTENT_BYTES = 4 * 1024 * 1024

export function readImageFiles(files: FileList | File[]): PromptAttachment[] {
  const list = Array.from(files)
  const images = list.filter((file) => file.type.startsWith('image/'))
  const out: PromptAttachment[] = []

  for (const file of images) {
    if (file.size > MAX_ATTACHMENT_CONTENT_BYTES) continue
    const previewUrl = URL.createObjectURL(file)
    out.push({
      id: crypto.randomUUID(),
      file,
      filename: file.name || 'Pasted image.png',
      mediaType: file.type || 'image/png',
      previewUrl,
    })
    if (out.length >= MAX_ATTACHMENTS) break
  }

  return out
}

function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => {
      const result = reader.result
      if (typeof result !== 'string') {
        reject(new Error('Failed to read attachment as data URL'))
        return
      }
      const comma = result.indexOf(',')
      resolve(comma >= 0 ? result.slice(comma + 1) : result)
    }
    reader.onerror = () => reject(reader.error ?? new Error('Failed to read attachment'))
    reader.readAsDataURL(file)
  })
}

export async function attachmentToWire(
  attachment: PromptAttachment
): Promise<{ filename: string; content: string; mediaType: string }> {
  return {
    filename: attachment.filename,
    content: await fileToBase64(attachment.file),
    mediaType: attachment.mediaType,
  }
}

export function revokeAttachmentPreviews(attachments: PromptAttachment[]) {
  for (const attachment of attachments) {
    URL.revokeObjectURL(attachment.previewUrl)
  }
}
