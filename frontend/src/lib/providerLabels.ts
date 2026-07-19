import type { ProviderAuthScheme, ProviderWireFormat } from '../services/types'

export function providerWireFormatLabel(value: ProviderWireFormat): string {
  switch (value) {
    case 'openai_chat_completions':
      return 'OpenAI Chat'
    case 'openai_responses':
      return 'OpenAI Responses'
    case 'anthropic_messages':
      return 'Anthropic Messages'
    case 'google_genai':
      return 'Google GenAI'
  }
}

export function providerAuthSchemeLabel(value: ProviderAuthScheme): string {
  switch (value) {
    case 'none':
      return 'No auth'
    case 'bearer':
      return 'Bearer'
    case 'x_api_key':
      return 'x-api-key'
    case 'x_goog_api_key':
      return 'x-goog-api-key'
  }
}
