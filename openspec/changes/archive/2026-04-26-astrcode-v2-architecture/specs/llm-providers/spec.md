## ADDED Requirements

### Requirement: LlmProvider trait
The system SHALL define an `LlmProvider` trait for LLM backend abstraction.
The trait SHALL expose: `generate()` returning a streaming response, and `model_limits()` returning context window and output limits.
Providers SHALL be registered lazily to avoid loading unused SDKs.

#### Scenario: OpenAI provider streams response
- **WHEN** `OpenAiProvider::generate()` is called with messages and tools
- **THEN** an HTTP request is sent to the OpenAI Chat Completions endpoint
- **THEN** SSE events are parsed and yielded as a Stream of LlmEvent

### Requirement: SSE stream parsing
The system SHALL parse Server-Sent Events from LLM providers.
Both Chat Completions format (`data: {...}`) and Responses format (`event:` + `data:`) SHALL be supported.
The parser SHALL handle UTF-8 multi-byte boundary splitting in stream chunks.

#### Scenario: Chat Completions SSE
- **WHEN** provider returns `data: {"choices":[{"delta":{"content":"Hello"}}]}\n\n`
- **THEN** parser yields LlmEvent::ContentDelta { text: "Hello" }

#### Scenario: Responses API event stream
- **WHEN** provider returns `event: response.output_text.delta\ndata: {"delta":"Hello"}\n\n`
- **THEN** parser yields LlmEvent::ContentDelta { text: "Hello" }

#### Scenario: Multi-byte UTF-8 split across chunks
- **WHEN** a UTF-8 character "世" (E4 B8 96) is split with E4 B8 in one chunk and 96 in the next
- **THEN** the parser buffers the incomplete bytes and correctly reassembles the character

### Requirement: Exponential backoff retry
The system SHALL retry failed LLM requests with exponential backoff.
Retryable status codes SHALL be: 408 (timeout), 429 (rate limit), 5xx (server errors).
Non-retryable errors (4xx except 408/429) SHALL fail immediately.

#### Scenario: Rate limit with retry
- **WHEN** provider returns 429 with Retry-After: 5
- **THEN** the client waits 5 seconds and retries
- **THEN** the retry succeeds

#### Scenario: Max retries exceeded
- **WHEN** provider returns 503 three times in a row
- **THEN** after the configured max_retries (default 3), the error is returned to the caller

### Requirement: Prompt caching awareness
The system SHALL track cache breakpoints for Anthropic/OpenAI prompt caching.
Cache hits and misses SHALL be logged for diagnostics.
Tool definitions SHALL be sorted for cache stability (tool ordering affects prompt cache keys).

#### Scenario: Cache hit detected
- **WHEN** provider reports cache read tokens > 0 on a system prompt with stable blocks
- **THEN** the CacheTracker logs cache hit with token savings

#### Scenario: Cache miss on prompt change
- **WHEN** a user-level instruction changes (e.g., project CLAUDE.md updated)
- **THEN** the CacheTracker detects the fingerprint change
- **THEN** the system prompt for the relevant layer is rebuilt

### Requirement: Provider configuration
Each provider SHALL be configurable with: api_base URL, api_key (or key source), model_id, extra_headers, and timeout.
Configuration SHALL support environment variable resolution for api_key.

#### Scenario: Custom API base
- **WHEN** user configures openai provider with api_base="https://custom.api.com/v1"
- **THEN** all LLM requests go to that base URL
- **THEN** the /chat/completions and /responses paths are appended as needed
