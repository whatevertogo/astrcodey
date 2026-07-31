import assert from 'node:assert/strict'
import {
  deriveThinkingFormValue,
  thinkingFormToRequest,
  effortOptions,
  isToggleOnlyThinking,
} from '../../target/frontend-settings-model-options/settingsSupport.js'

// ── deriveThinkingFormValue ──

{
  // No capability → default
  const result = deriveThinkingFormValue(undefined, undefined)
  assert.deepEqual(result, { mode: 'default', effort: '', budgetTokens: '' })
}

{
  // Capability, no current thinking → default
  const result = deriveThinkingFormValue(
    {
      allowedEffort: ['low', 'medium', 'high'],
      budgetMin: null,
      budgetMax: null,
    },
    undefined
  )
  assert.deepEqual(result, { mode: 'default', effort: '', budgetTokens: '' })
}

{
  // Capability, thinking null → default
  const result = deriveThinkingFormValue(
    {
      allowedEffort: ['low', 'medium', 'high'],
      budgetMin: null,
      budgetMax: null,
    },
    null
  )
  assert.deepEqual(result, { mode: 'default', effort: '', budgetTokens: '' })
}

{
  // Capability, thinking enabled → enabled with effort and budgetTokens
  const result = deriveThinkingFormValue(
    {
      allowedEffort: ['low', 'medium', 'high'],
      budgetMin: null,
      budgetMax: null,
    },
    { enabled: true, effort: 'medium', budgetTokens: 4096 }
  )
  assert.deepEqual(result, {
    mode: 'enabled',
    effort: 'medium',
    budgetTokens: '4096',
  })
}

{
  // Capability, thinking enabled, no effort/budget → enabled with empty strings
  const result = deriveThinkingFormValue(
    {
      allowedEffort: null,
      budgetMin: null,
      budgetMax: null,
    },
    { enabled: true }
  )
  assert.deepEqual(result, { mode: 'enabled', effort: '', budgetTokens: '' })
}

{
  // Capability, thinking disabled → disabled
  const result = deriveThinkingFormValue(
    {
      allowedEffort: null,
      budgetMin: null,
      budgetMax: null,
    },
    { enabled: false }
  )
  assert.deepEqual(result, { mode: 'disabled', effort: '', budgetTokens: '' })
}

{
  // A capability that cannot be disabled falls back to the model default.
  const result = deriveThinkingFormValue(
    {
      allowedEffort: ['low', 'medium', 'high'],
      budgetMin: null,
      budgetMax: null,
      canDisable: false,
    },
    { enabled: false }
  )
  assert.deepEqual(result, { mode: 'default', effort: '', budgetTokens: '' })
}

// ── thinkingFormToRequest ──

{
  // Default mode → no thinking field
  const req = thinkingFormToRequest('defaultProfile', 'gpt-4', {
    mode: 'default',
    effort: '',
    budgetTokens: '',
  })
  assert.equal(req.profileName, 'defaultProfile')
  assert.equal(req.modelId, 'gpt-4')
  assert.equal(req.thinking, undefined)
}

{
  // Disabled mode → { enabled: false }
  const req = thinkingFormToRequest(
    'p',
    'm',
    {
      mode: 'disabled',
      effort: '',
      budgetTokens: '',
    },
    { allowedEffort: [], canDisable: true }
  )
  assert.deepEqual(req.thinking, { enabled: false })
}

{
  assert.throws(
    () =>
      thinkingFormToRequest('p', 'm', {
        mode: 'enabled',
        effort: '',
        budgetTokens: '',
      }),
    /未声明 Thinking 能力/
  )
}

{
  assert.throws(
    () =>
      thinkingFormToRequest(
        'p',
        'm',
        { mode: 'disabled', effort: '', budgetTokens: '' },
        {
          allowedEffort: ['low', 'medium', 'high'],
          canDisable: false,
        }
      ),
    /不支持关闭 Thinking/
  )
}

{
  // Enabled mode with effort and budgetTokens
  const req = thinkingFormToRequest(
    'p',
    'm',
    {
      mode: 'enabled',
      effort: 'high',
      budgetTokens: '8192',
    },
    {
      allowedEffort: ['low', 'medium', 'high'],
      budgetMin: 1024,
      budgetMax: 16384,
    }
  )
  assert.deepEqual(req.thinking, {
    enabled: true,
    effort: 'high',
    budgetTokens: 8192,
  })
}

{
  // Enabled mode with effort only
  const req = thinkingFormToRequest(
    'p',
    'm',
    {
      mode: 'enabled',
      effort: 'low',
      budgetTokens: '',
    },
    { allowedEffort: ['low', 'medium', 'high'] }
  )
  assert.deepEqual(req.thinking, { enabled: true, effort: 'low' })
}

{
  // Toggle-only capability needs no effort or budget
  const req = thinkingFormToRequest(
    'p',
    'm',
    { mode: 'enabled', effort: '', budgetTokens: '' },
    { allowedEffort: [] }
  )
  assert.deepEqual(req.thinking, { enabled: true })
}

{
  // Omitted allowedEffort accepts an optional provider-specific effort value.
  const req = thinkingFormToRequest(
    'p',
    'm',
    { mode: 'enabled', effort: 'xhigh', budgetTokens: '' },
    { canDisable: true }
  )
  assert.deepEqual(req.thinking, { enabled: true, effort: 'xhigh' })
}

{
  assert.throws(
    () =>
      thinkingFormToRequest(
        'p',
        'm',
        { mode: 'enabled', effort: 'high', budgetTokens: '' },
        { allowedEffort: [], canDisable: true }
      ),
    /不支持设置思考努力层级/
  )
}

{
  assert.throws(
    () =>
      thinkingFormToRequest(
        'p',
        'm',
        { mode: 'enabled', effort: '', budgetTokens: '4096' },
        { allowedEffort: [], canDisable: true }
      ),
    /不支持设置思考预算 Token/
  )
}

{
  assert.throws(
    () =>
      thinkingFormToRequest(
        'p',
        'm',
        { mode: 'enabled', effort: '', budgetTokens: '' },
        { allowedEffort: ['low', 'high'] }
      ),
    /请选择思考努力层级/
  )
}

{
  assert.throws(
    () =>
      thinkingFormToRequest(
        'p',
        'm',
        { mode: 'enabled', effort: '', budgetTokens: '' },
        { allowedEffort: [], budgetMin: 1024, budgetMax: 64000 }
      ),
    /请输入思考预算 Token/
  )
}

{
  assert.throws(
    () =>
      thinkingFormToRequest(
        'p',
        'm',
        { mode: 'enabled', effort: '', budgetTokens: 'abc' },
        { allowedEffort: [], budgetMin: 1024 }
      ),
    /必须是正整数/
  )
}

{
  assert.throws(
    () =>
      thinkingFormToRequest(
        'p',
        'm',
        { mode: 'enabled', effort: '', budgetTokens: '512' },
        { allowedEffort: [], budgetMin: 1024 }
      ),
    /不能小于 1024/
  )
}

// ── effortOptions ──

{
  const options = effortOptions(['low', 'medium', 'high'])
  assert.deepEqual(options, [
    { label: '低', value: 'low' },
    { label: '中', value: 'medium' },
    { label: '高', value: 'high' },
  ])
}

{
  // Unknown value falls back to raw string
  const options = effortOptions(['turbo'])
  assert.deepEqual(options, [{ label: 'turbo', value: 'turbo' }])
}

// ── isToggleOnlyThinking ──

{
  // null → false
  assert.equal(isToggleOnlyThinking(null), false)
}

{
  // Has effort → not toggle-only
  assert.equal(
    isToggleOnlyThinking({
      allowedEffort: ['low'],
      budgetMin: null,
      budgetMax: null,
    }),
    false
  )
}

{
  // Has budget → not toggle-only
  assert.equal(
    isToggleOnlyThinking({
      allowedEffort: null,
      budgetMin: 1024,
      budgetMax: null,
    }),
    false
  )
}

{
  // Omitted allowedEffort means provider-specific effort values are accepted.
  assert.equal(
    isToggleOnlyThinking({
      allowedEffort: null,
      budgetMin: null,
      budgetMax: null,
    }),
    false
  )
}

{
  // Empty array for allowedEffort, no budget → toggle-only
  assert.equal(
    isToggleOnlyThinking({
      allowedEffort: [],
      budgetMin: null,
      budgetMax: null,
    }),
    true
  )
}

console.log('settings model options thinking tests passed')
