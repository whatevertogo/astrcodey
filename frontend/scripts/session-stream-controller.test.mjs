import assert from 'node:assert/strict'
import {
  SessionStreamController,
  SessionStreamProtocolError,
} from '../../target/frontend-session-stream/sessionStreamController.js'

function deferred() {
  let resolve
  const promise = new Promise((next) => {
    resolve = next
  })
  return { promise, resolve }
}

function testScheduler() {
  const timers = []
  return {
    timers,
    scheduler: {
      schedule(callback, delayMs) {
        const timer = { callback, delayMs, cancelled: false }
        timers.push(timer)
        return timer
      },
      cancel(timer) {
        timer.cancelled = true
      },
      reconnectDelayMs(attempt) {
        return 1000 * (attempt + 1)
      },
    },
    runNext() {
      const timer = timers.find((candidate) => !candidate.cancelled)
      assert.ok(timer, 'expected a scheduled reconnect')
      timer.cancelled = true
      timer.callback()
    },
  }
}

async function flushMicrotasks() {
  await Promise.resolve()
  await Promise.resolve()
}

{
  const connection = deferred()
  const calls = []
  const statuses = []
  const scheduler = testScheduler()

  const controller = new SessionStreamController({
    sessionId: 'session-1',
    initialCursor: '1',
    consume: async (sessionId, cursor, onEnvelope, signal, onOpen) => {
      calls.push({ sessionId, cursor, signal })
      onOpen()
      onEnvelope({
        sessionId,
        cursor: { value: '3' },
        delta: {
          kind: 'updateControlState',
          control: {
            phase: 'idle',
            canSubmitPrompt: true,
            canRequestCompact: true,
            compactPending: false,
            compacting: false,
          },
        },
      })
      signal.addEventListener('abort', () => connection.resolve('aborted'), {
        once: true,
      })
      return connection.promise
    },
    host: {
      isActive: () => true,
      applyEnvelope: () => undefined,
      rehydrate: async () => null,
      updateStatus: (status, error) => statuses.push({ status, error }),
    },
    scheduler: scheduler.scheduler,
  })

  controller.start()
  await flushMicrotasks()
  assert.deepEqual(
    statuses.map(({ status }) => status),
    ['connecting', 'connected']
  )

  connection.resolve('ended')
  await flushMicrotasks()
  assert.equal(statuses.at(-1).status, 'reconnecting')
  assert.equal(scheduler.timers[0].delayMs, 1000)

  controller.stop()
  assert.equal(scheduler.timers[0].cancelled, true)
  scheduler.timers[0].callback()
  await flushMicrotasks()
  assert.equal(calls.length, 1, 'stopped controller must not reconnect')
}

{
  const calls = []
  const statuses = []
  const scheduler = testScheduler()
  const recoveredConnection = deferred()
  let rehydrateCount = 0

  const controller = new SessionStreamController({
    sessionId: 'session-2',
    initialCursor: '4',
    consume: async (sessionId, cursor, _onEnvelope, signal, onOpen) => {
      calls.push({ cursor, signal })
      if (calls.length === 1) {
        throw new SessionStreamProtocolError('invalid conversation delta')
      }
      onOpen()
      signal.addEventListener(
        'abort',
        () => recoveredConnection.resolve('aborted'),
        { once: true }
      )
      return recoveredConnection.promise
    },
    host: {
      isActive: () => true,
      applyEnvelope: () => undefined,
      rehydrate: async () => {
        rehydrateCount += 1
        return '9'
      },
      updateStatus: (status, error) => statuses.push({ status, error }),
    },
    scheduler: scheduler.scheduler,
  })

  controller.start()
  await flushMicrotasks()
  assert.equal(statuses.at(-1).status, 'degraded')
  assert.equal(scheduler.timers[0].delayMs, 2000)

  scheduler.runNext()
  await flushMicrotasks()
  assert.equal(rehydrateCount, 1)
  assert.equal(calls[1].cursor, '9')
  assert.equal(statuses.at(-1).status, 'connected')

  controller.stop()
  assert.equal(calls[1].signal.aborted, true)
}

console.log('session stream controller contract tests passed')
