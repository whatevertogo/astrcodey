import assert from 'node:assert/strict'

import {
  MAX_TIMELINE_PAGES,
  earliestConversationCursor,
  latestTimelineWindow,
  prependTimelinePage,
} from '../../target/frontend-conversation-history/conversationHistory.js'

const block = (id, text = id) => ({
  kind: 'user',
  id,
  text,
  attachments: [],
})

assert.equal(earliestConversationCursor('9', '10'), '9')
assert.equal(earliestConversationCursor('101', '100'), '100')

let window = latestTimelineWindow(
  [block('durable')],
  [block('durable', 'updated'), block('transient')]
)
assert.deepEqual(
  window.blocks.map((item) => [item.id, item.text]),
  [
    ['durable', 'updated'],
    ['transient', 'transient'],
  ]
)

window = prependTimelinePage(window, [block('older'), block('durable')])
assert.deepEqual(
  window.blocks.map((item) => item.id),
  ['older', 'durable', 'transient']
)

for (let page = 2; page <= MAX_TIMELINE_PAGES; page += 1) {
  window = prependTimelinePage(window, [block(`older-${page}`)])
}

assert.equal(window.pageBlockIds.length, MAX_TIMELINE_PAGES)
assert.equal(window.detachedFromLatest, true)
assert.equal(window.blocks[0].id, `older-${MAX_TIMELINE_PAGES}`)
assert.equal(
  window.blocks.some((item) => item.id === 'durable'),
  false,
  'the newest evicted page must release its blocks'
)
