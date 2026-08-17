# deepseek-harness 扩展系统对外 API 签名清单

> 来源：`/Users/whatevertogo/githubown/deepseek-harness`(TypeScript monorepo)。签名均逐字照抄源码；为可读性省略了 JSDoc 注释与私有成员（私有成员在 `private` 标注后略去）。每个条目注明文件路径。

## 1. `vendor/cordis/` — Context 核心 API

### `vendor/cordis/src/context.ts`

```ts
export interface Context {
  [symbols.isolate]: Dict<symbol>
  [symbols.intercept]: Dict
  root: this
  baseUrl?: string
  events: EventsService
  logger: LoggerService
  reflect: ReflectService
  registry: RegistryService
}

export class Context {
  static readonly effect: unique symbol = symbols.effect
  static readonly filter: unique symbol = symbols.filter
  static readonly isolate: unique symbol = symbols.isolate
  static readonly intercept: unique symbol = symbols.intercept

  static is(value: any): value is Context

  constructor()

  extend(meta = {}): this
  isolate(name: string, label?: symbol)
  intercept<K extends InjectKey>(name: K, config: Context[K] extends { [symbols.config]: infer T } ? T : never): this
  intercept(name: string, config: any): this
}
```

### `vendor/cordis/src/events.ts` — 事件方法(declare module 混入 Context)

```ts
export function isBailed(value: any)

export type Parameters<F> = F extends (...args: infer P) => any ? P : never
export type ReturnType<F> = F extends (...args: any) => infer R ? R : never
export type ThisType<F> = F extends (this: infer T, ...args: any) => any ? T : never

export type DispatchMode = 'emit' | 'parallel' | 'serial' | 'bail' | 'waterfall'

declare module './context.ts' {
  export interface Context {
    parallel<K extends keyof Events>(name: K, ...args: Parameters<Events[K]>): Promise<void>
    parallel<K extends keyof Events>(thisArg: NoInfer<ThisType<Events[K]>>, name: K, ...args: Parameters<Events[K]>): Promise<void>
    emit<K extends keyof Events>(name: K, ...args: Parameters<Events[K]>): void
    emit<K extends keyof Events>(thisArg: NoInfer<ThisType<Events[K]>>, name: K, ...args: Parameters<Events[K]>): void
    serial<K extends keyof Events>(name: K, ...args: Parameters<Events[K]>): Promisify<ReturnType<Events[K]>>
    serial<K extends keyof Events>(thisArg: NoInfer<ThisType<Events[K]>>, name: K, ...args: Parameters<Events[K]>): Promisify<ReturnType<Events[K]>>
    bail<K extends keyof Events>(name: K, ...args: Parameters<Events[K]>): ReturnType<Events[K]>
    bail<K extends keyof Events>(thisArg: NoInfer<ThisType<Events[K]>>, name: K, ...args: Parameters<Events[K]>): ReturnType<Events[K]>
    waterfall<K extends keyof Events>(name: K, ...args: Parameters<Events[K]>): ReturnType<Events[K]>
    waterfall<K extends keyof Events>(thisArg: NoInfer<ThisType<Events[K]>>, name: K, ...args: Parameters<Events[K]>): ReturnType<Events[K]>
    on<K extends keyof Events>(name: K, listener: Events[K], options?: boolean | EventOptions): () => boolean
    once<K extends keyof Events>(name: K, listener: Events[K], options?: boolean | EventOptions): () => boolean
  }
}

export interface EventOptions {
  prepend?: boolean
  global?: boolean
}

export interface Hook extends EventOptions {
  ctx: Context
  callback: (...args: any[]) => any
}

export class EventsService {
  _hooks: Record<keyof any, Hook[]>
  constructor(private ctx: Context)
  dispatch(type: string, args: any[])
  async parallel(...args: any[])
  emit(...args: any[])
  async serial(...args: any[])
  bail(...args: any[])
  waterfall(...args: any[])
  register(label: string, hooks: Hook[], callback: any, options: EventOptions): () => void
  unregister(hooks: Hook[], callback: any)
  on(name: string | symbol, listener: (...args: any) => any, options?: boolean | EventOptions)
  once(name: string, listener: (...args: any) => any, options?: boolean | EventOptions)
}

// 内置事件
export interface Events {
  'internal/plugin'(fiber: Fiber): void
  'internal/status'(fiber: Fiber, oldValue: FiberState): void
  'internal/config'(this: Fiber, config: any, next: () => any): any          // waterfall
  'internal/service'(this: Context, name: string, value: any): void
  'internal/update'(this: Fiber, config: any, noSave: boolean, next: () => void | Promise<void>): void | Promise<void> // waterfall
  'internal/get'(ctx: Context, name: string, error: Error, next: () => any): any       // waterfall
  'internal/set'(ctx: Context, name: string, value: any, error: Error, next: () => boolean): boolean // waterfall
  'internal/listener'(this: Context, name: string, listener: any, prepend: boolean): void // bail
  'internal/dispatch'(mode: DispatchMode, name: string, args: any[], thisArg: any): void
}
```

### `vendor/cordis/src/registry.ts` — plugin/inject

```ts
export type Inject<M = Dict> = (keyof M)[] | { [K in keyof M]?: M[K] }

export type InjectKey = keyof {
  [K in keyof Context & string as Context[K] extends { [symbols.config]: any } ? K : never]: any
}

export function Inject<K extends InjectKey>(name: K, config?: Context[K] extends { [symbols.config]: infer T } ? T : never)

export namespace Inject {
  export function resolve(inject: Inject | null | undefined, result: Dict = Object.create(null))
}

export type Plugin<T = any> =
  | Plugin.Function<T>
  | Plugin.Constructor<T>
  | Plugin.Object<T>

export namespace Plugin {
  export interface Base<T = any> {
    name?: string
    Config?: StandardSchemaV1<any, T>
    inject?: Inject
    provide?: string | string[]
    intercept?: Dict<boolean>
  }
  export interface Transform<S, T> {
    schema?: true
    Config: (config: S) => T
  }
  export interface Function<T = any> extends Base<T> {
    (ctx: Context, config: T): any
  }
  export interface Constructor<T = any> extends Base<T> {
    new (ctx: Context, config: T): any
  }
  export interface Object<T = any> extends Base<T> {
    apply(ctx: Context, config: T): any
  }
  export interface Runtime {
    name?: string
    fibers: DisposableList<Fiber>
    callback: globalThis.Function
    Config?: StandardSchemaV1
  }
}

declare module './context.ts' {
  export interface Context {
    inject(deps: Inject, callback: Plugin.Function<void>): Fiber & PromiseLike<Fiber>
    plugin<P extends Plugin>(plugin: P, ...args: Spread<GetPluginConfig<P>>): Fiber & PromiseLike<Fiber>
  }
}

export class RegistryService {
  constructor(public ctx: Context)
  get counter()
  get size()
  resolve(plugin: Plugin): Function | undefined
  get(plugin: Plugin)
  has(plugin: Plugin)
  delete(plugin: Plugin)
  keys()
  values()
  entries()
  forEach(callback: (value: Plugin.Runtime, key: Function) => void)
  inject(inject: Inject, callback: Plugin.Function<void>)
  plugin(plugin: Plugin, config?: any, getOuterStack = buildOuterStack())
}
```

### `vendor/cordis/src/reflect.ts` — get/set/provide/accessor/mixin

```ts
declare module './context.ts' {
  interface Context {
    get<K extends string & keyof this>(name: K, strict?: boolean): undefined | this[K]
    get(name: string, strict?: boolean): any
    set<K extends string & keyof this>(name: K, value: undefined | this[K]): void
    set(name: string, value: any): void
    provide<K extends string & keyof this>(name: K, value: undefined | this[K]): () => void
    provide(name: string, value?: any): () => void
    accessor(name: string, options: Omit<Property.Accessor, 'type'>): void
    mixin<K extends string & keyof this>(name: K, mixins: (keyof this & keyof this[K])[] | Dict<string>): void
    mixin<T extends {}>(source: T, mixins: (keyof this & keyof T)[] | Dict<string>): void
  }
}

export type Property = Property.Service | Property.Accessor

export namespace Property {
  export interface Service {
    type: 'service'
  }
  export interface Accessor {
    type: 'accessor'
    get: (this: Context, receiver: any, error: Error) => any
    set?: (this: Context, value: any, receiver: any, error: Error) => boolean
  }
}

export interface Impl {
  name: string
  fiber: Fiber
  value?: any
  check?: () => boolean
}

export class ReflectService {
  static handler: ProxyHandler<Context>
  public store: Dict<Impl, symbol>
  public props: Dict<Property>
  constructor(public ctx: Context)
  get(name: string, strict = true)
  set(name: string, value: any, error?: Error)
  provide(name: string, value?: any, check?: () => boolean)
  notify(names: string[], filter = (ctx: Context, name: string) => ...)
  accessor(name: string, options: Omit<Property.Accessor, 'type'>)
  mixin(source: any, mixins: string[] | Dict<string>)
  trace<T>(value: T)
  bind<T extends Function>(callback: T)
}
```

混入来源（`ReflectService` 构造函数）:

```ts
this.mixin('reflect', ['get', 'set', 'provide', 'accessor', 'mixin'])
this.mixin('fiber', ['runtime', 'effect'])
this.mixin('registry', ['inject', 'plugin'])
this.mixin('events', ['on', 'once', 'parallel', 'emit', 'serial', 'bail', 'waterfall'])
```

### `vendor/cordis/src/fiber.ts` — effect / fiber

```ts
declare module './context.ts' {
  export interface Context extends Pick<Fiber, 'effect'> {
    fiber: Fiber
  }
}

export class ValidationError extends TypeError {
  constructor(issues: readonly StandardSchemaV1.Issue[])
}

export function resolveConfig(runtime: Plugin.Runtime, config: any)

export type Disposable<T = any> = () => T

export type Effect<T = any> =
  | SyncEffect<T>
  | AsyncEffect<T>

type SyncEffect<T = any> =
  | Disposable<T>
  | Iterable<Disposable<T>, void, void>

type AsyncEffect<T = any> =
  | Promise<Disposable<T>>
  | AsyncIterable<Disposable<T>, void, void>

export interface EffectMeta {
  label: string
  children: EffectMeta[]
}

export const enum FiberState {
  PENDING,
  LOADING,
  ACTIVE,
  FAILED,
  DISPOSED,
  UNLOADING,
}

export class CordisError extends Error {
  constructor(public code: CordisError.Code, message?: string)
}
export namespace CordisError {
  export type Code = keyof typeof Code
  export const Code = {
    INACTIVE_EFFECT: 'cannot create effect on inactive context',
  } as const
}

export class Fiber {
  public uid: number | null
  public readonly ctx: Context
  public config: any
  public _config: any
  public state = FiberState.PENDING
  public readonly dispose: () => Promise<void>
  public store: Dict<Impl> | undefined
  public inertia: Promise<void> | undefined

  constructor(
    public parent: Context,
    config: any,
    public inject: Dict<any>,
    public runtime: Plugin.Runtime | null,
    getOuterStack: () => string[],
  )

  get name()

  assertActive(): void

  effect(execute: () => SyncEffect, label?: string): Disposable<Promise<void>>
  effect(execute: () => Effect, label?: string): AsyncDisposable<Promise<void>>

  getEffects(): EffectMeta[]

  async await(): Promise<this>
  async restart(): Promise<void>
  update(config: any, noSave = false)
}
```

### `vendor/cordis/src/logger.ts`

```ts
export type LoggerType = 'error' | 'info' | 'warn' | 'debug'
export type LoggerMethod = (format: any, ...param: any[]) => void
export type Formatter = (value: any, exporter: Exporter, message: Message) => any

export const enum LoggerLevel {
  ERROR = 0,
  INFO = 1,
  WARN = 2,
  DEBUG = 3,
}

export interface Message {
  sn: number
  ts: number
  name: string
  type: LoggerType
  level: number
  args: any[]
  fiber?: WeakRef<Fiber>
}

export interface Exporter {
  colors?: number | false
  maxLength?: number
  levels?: Record<string, number>
  formatters?: Record<string, Formatter>
  export(message: Message): void
}

export interface LoggerOptions {
  name: string
  meta?: Partial<Message>
  level?: number
}

export interface Logger extends LoggerOptions {}
export interface Logger extends Record<LoggerType, LoggerMethod> {}

export class Logger {
  static color(exporter: Exporter, code: number, value: any, decoration = '')
  static code(name: string, level?: false | number)
  static format(exporter: Exporter, message: Message): string
  constructor(options: LoggerOptions, private service: LoggerService)
}

export namespace LoggerService {
  export interface Intercept {
    name?: string
    level?: number
  }
}

// ctx.logger 是可调用服务
export interface LoggerService extends Record<LoggerType, LoggerMethod> {
  (name?: string): Logger
}

export class LoggerService {
  bufferSize = 1000
  buffer: Message[] = []
  ctx!: Context
  exporters = new Map<number, Exporter>()
  constructor(ctx: Context)
  exporter(exporter: Exporter)
}
```

### `vendor/cordis/src/service.ts` — Service 基类

```ts
export abstract class Service<out T = never> {
  static readonly init: unique symbol = symbols.init
  static readonly check: unique symbol = symbols.check
  static readonly config: unique symbol = symbols.config
  static readonly invoke: unique symbol = symbols.invoke
  static readonly extend: unique symbol = symbols.extend
  static readonly tracker: unique symbol = symbols.tracker
  static readonly resolveConfig: unique symbol = symbols.resolveConfig

  declare [symbols.config]: T
  public name!: string

  constructor(protected ctx: Context, name: string)

  protected [symbols.filter](ctx: Context)
  protected [symbols.extend](props?: any)

  static [Symbol.hasInstance](instance: any)
}
```

---

## 2. `packages/core/tools/` — ToolRuntime

### `packages/core/tools/src/index.ts`

Context/事件增强：

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    tools: ToolRuntime
  }

  interface Events {
    'tools/pre-execute'(this: Scoped<ToolRuntime>, exec: ToolExecution, next: () => Promise<PreToolDecision>): Promise<PreToolDecision> // waterfall
    'tools/execute'(this: Scoped<ToolRuntime>, exec: ToolDispatchExecution, next: () => Promise<ToolExecutionResult>): Promise<ToolExecutionResult> // waterfall
    'tools/post-execute'(this: Scoped<ToolRuntime>, exec: ToolExecution, result: Readonly<ToolExecutionResult>, next: () => Promise<PostToolDecision>): Promise<PostToolDecision> // waterfall
    'tools/code-dispatch-log'(this: Scoped<ToolRuntime>, dispatch: CodeDispatchLog, next: () => Promise<ContentBlock[]>): Promise<ContentBlock[]> // waterfall
    'tools/result'(this: Scoped<ToolRuntime>, exec: Readonly<ToolExecution>, result: Readonly<ToolExecutionResult>): undefined // emit
    'tools/change'(): void // emit
  }
}
```

核心类型：

```ts
export interface ToolOutputDefinition {
  readonly schema: JsonSchemaNode
  render(args: unknown, value: JsonValue): ContentBlock[]
  presentationMeta?(args: unknown, value: JsonValue): JsonValue
}

export interface ToolDefinition extends ToolSchema {
  readonly output: ToolOutputDefinition
  execute(args: unknown, exec: ToolRunContext): Promise<unknown>
  finalizeContent?(exec: Readonly<ToolExecution>, result: Readonly<ToolExecutionResult>): ContentBlock[] | undefined
  timeoutMs?: number
  isConcurrencySafe?(args: unknown): boolean
  presentCall?(args: unknown): ToolCallView | undefined
  presentResult?(args: unknown, result: ToolResult): ToolResultView | undefined
}

export interface ToolResult {
  content: ContentBlock[]
  isError: boolean
  meta?: JsonValue
}

export type ToolExecutionToken = symbol & { readonly [toolExecutionTokenBrand]: true }

export interface ToolExecutionInput {
  readonly callId: CallId
  readonly rootCallId?: CallId
  readonly name: string
  readonly arguments: unknown
  readonly agent?: Agent
  readonly parent?: ToolExecutionToken
  readonly signal: AbortSignal
}

export type ToolExecutionMode =
  | { kind: 'parallel' }
  | { kind: 'exclusive' }

export interface CodeDispatchLog {
  readonly exec: ToolExecution
  readonly agent?: Agent
  readonly subCallId: CallId
  readonly name: string
  readonly isError: boolean
  readonly content: ContentBlock[]
}

export interface ToolExecution extends ToolExecutionInput {
  readonly rootCallId: CallId
  readonly token: ToolExecutionToken
}

export interface ToolDispatchExecution extends Omit<ToolExecution, 'signal'> {
  signal: AbortSignal
}

export interface ToolRunContext extends ToolExecution {
  deferContext(context: UserMessage): void
  concludeTurn(): void
}

export type ScheduledToolPreparation =
  | { kind: 'dispatch'; exec: ToolRunContext }
  | { kind: 'post-result'; exec: ToolRunContext; result: ToolExecutionResult }
  | { kind: 'final-result'; exec: ToolRunContext; result: ToolExecutionResult }

export type ScheduledToolDispatch =
  | { kind: 'post-result'; result: ToolExecutionResult }
  | { kind: 'final-result'; result: ToolExecutionResult }

/** @internal */
export interface ToolRuntimeScheduler {
  prepare(exec: ToolExecutionInput): Promise<ScheduledToolPreparation>
  dispatch(exec: ToolRunContext): Promise<ScheduledToolDispatch>
  finalize(exec: ToolRunContext, result: ToolExecutionResult): Promise<ToolExecutionResult>
  finish(exec: ToolRunContext, result: ToolExecutionResult): ToolExecutionResult
}

export const TOOL_RUNTIME_SCHEDULER: unique symbol = Symbol('@deepseek-ai/dsh-tools.scheduler')

export const TOOL_ABORTED = 'ABORTED'
export const TOOL_ABORTED_BEFORE_DISPATCH = 'ABORTED_BEFORE_DISPATCH'

export interface ToolErrorInfo {
  name: string
  code: string
}

export interface ToolFailure {
  message: string
  info?: ToolErrorInfo
}

export class ToolNotFoundError extends HarnessError {
  constructor(toolName: string, reachableFrom?: string)
}

export class ToolOutputError extends HarnessError {
  readonly violations: string[]
  constructor(toolName: string, violations: string[])
}

export interface ToolExecutionSuccess {
  readonly isError: false
  readonly value: JsonValue
  readonly content: ContentBlock[]
  readonly error?: never
  readonly meta?: JsonValue
  readonly additionalContexts?: UserMessage[]
  readonly concludesTurn?: true
}

export interface ToolExecutionFailure {
  readonly isError: true
  readonly error: ToolFailure
  readonly value?: never
  readonly content: ContentBlock[]
  readonly meta?: JsonValue
  readonly additionalContexts?: UserMessage[]
  readonly concludesTurn?: never
}

export type ToolExecutionResult = ToolExecutionSuccess | ToolExecutionFailure

export type PreToolDecision =
  | { kind: 'allow' }
  | { kind: 'deny'; reason: string }
  | { kind: 'ask'; reason?: string }

export type PostToolDecision =
  | { kind: 'accept'; content?: ContentBlock[]; value?: never; additionalContexts?: UserMessage[] }
  | { kind: 'accept'; value: JsonValue; content?: never; additionalContexts?: UserMessage[] }
  | { kind: 'block'; feedback: ContentBlock[]; additionalContexts?: UserMessage[] }

export type ToolPresentationMode = 'native' | 'code' | 'both'

export interface Config {
  mode?: ToolPresentationMode
  maxParallelSubCalls?: number
}

export interface ToolRestriction {
  readonly allow?: readonly string[]
  readonly deny?: readonly string[]
}

export type ToolGuard = (execution: Readonly<ToolExecution>) => string | undefined
```

`ToolRuntime`(public 方法全量）:

```ts
export class ToolRuntime extends Service {
  static inject = ['systemPrompt']

  static Config: z<Config> = z.object({
    mode: z.union(['native', 'code', 'both'] as const).default('native'),
    maxParallelSubCalls: z.natural().min(1).default(10),
  })

  /** @internal */
  readonly [TOOL_RUNTIME_SCHEDULER]: ToolRuntimeScheduler

  constructor(ctx: Context, config: Config = {})

  presentAs(mode: ToolPresentationMode): () => void
  register(definition: ToolDefinition): () => void
  restrict(filter: ToolRestriction): () => void
  guard(guard: ToolGuard): () => void
  get(name: string, scope?: ScopeKey): ToolDefinition | undefined
  schemas(scope?: ScopeKey): ToolSchema[]
  executionMode(exec: ToolExecutionInput): ToolExecutionMode
  async execute(exec: ToolExecutionInput): Promise<ToolExecutionResult>
}
```

### `packages/core/tools/src/schema.ts`

```ts
export interface ValueSchemaAnnotations {
  description?: string
  title?: string
  default?: JsonValue
  examples?: JsonValue
}

export interface StringValueSchemaSpec extends ValueSchemaAnnotations {
  type: 'string'
  enum?: readonly string[]
  const?: string
}
export interface NumberValueSchemaSpec extends ValueSchemaAnnotations {
  type: 'number'
  enum?: readonly number[]
  const?: number
}
export interface IntegerValueSchemaSpec extends ValueSchemaAnnotations {
  type: 'integer'
  enum?: readonly number[]
  const?: number
}
export interface BooleanValueSchemaSpec extends ValueSchemaAnnotations {
  type: 'boolean'
  enum?: readonly boolean[]
  const?: boolean
}
export interface NullValueSchemaSpec extends ValueSchemaAnnotations {
  type: 'null'
  enum?: readonly null[]
  const?: null
}
export interface ArrayValueSchemaSpec extends ValueSchemaAnnotations {
  type: 'array'
  items?: ValueSchemaSpec
}
export interface ObjectValueSchemaSpec extends ValueSchemaAnnotations {
  type: 'object'
  properties?: ParameterSchemaSpec
  additionalProperties: boolean
}
export interface JsonValueSchemaSpec extends ValueSchemaAnnotations {
  type: 'json'
}
export interface OneOfValueSchemaSpec extends ValueSchemaAnnotations {
  oneOf: readonly [ValueSchemaSpec, ValueSchemaSpec, ...ValueSchemaSpec[]]
}

export type ValueSchemaSpec =
  | StringValueSchemaSpec
  | NumberValueSchemaSpec
  | IntegerValueSchemaSpec
  | BooleanValueSchemaSpec
  | NullValueSchemaSpec
  | ArrayValueSchemaSpec
  | ObjectValueSchemaSpec
  | JsonValueSchemaSpec
  | OneOfValueSchemaSpec

export type ParameterPropertySpec = ValueSchemaSpec & { required?: true }

export type ParameterSchemaSpec = {
  [key: string]: ParameterPropertySpec
  [key: symbol]: never
}

export interface ParameterJsonSchema extends ObjectJsonSchema {
  properties: Record<string, JsonSchemaNode>
}

export type InferValue<S> = InferValueAt<S, []>
export type InferArgs<S> = InferProperties<S, []>

export function valueSchemaSpecToJsonSchema(spec: ValueSchemaSpec): JsonSchemaNode
export function parameterSchemaSpecToJsonSchema(spec: ParameterSchemaSpec): ParameterJsonSchema

export class ToolArgsError extends HarnessError {
  readonly violations: string[]
  constructor(violations: string[])
}

export function validateArgs(spec: ParameterSchemaSpec, args: unknown): string[]

export interface DefineToolOptions<S extends ParameterSchemaSpec, O extends ValueSchemaSpec> {
  readonly name: string
  readonly description: string
  readonly parameters: S
  readonly output: {
    readonly schema: O
    render(args: InferArgs<S>, value: InferValue<NoInfer<O>>): ContentBlock[]
    presentationMeta?(args: InferArgs<S>, value: InferValue<NoInfer<O>>): JsonValue
  }
  readonly timeoutMs?: number
  isConcurrencySafe?(args: InferArgs<S>): boolean
  execute(args: InferArgs<S>, exec: ToolRunContext): Promise<InferValue<NoInfer<O>>>
  finalizeContent?(exec: Readonly<ToolExecution>, result: Readonly<ToolExecutionResult>): ContentBlock[] | undefined
  presentCall?(args: InferArgs<S>): ToolCallView | undefined
  presentResult?(args: InferArgs<S>, result: ToolResult): ToolResultView | undefined
}

export function defineTool<const S extends ParameterSchemaSpec, const O extends ValueSchemaSpec>(
  options: DefineToolOptions<S, O>,
): ToolDefinition
```

---

## 3. `packages/core/agent/`

### `packages/core/agent/src/runtime-types.ts`

```ts
export interface AgentOptions {
  provider?: string
  model?: string
  maxTokens?: number
}

export interface CancelOptions {
  keepInbox?: boolean | undefined
}

export type AgentStatus = 'idle' | 'running'

export type PreStepDecision =
  | { kind: 'reject' }
  | { kind: 'enter'; messages: UserMessage[] }

export type RequestErrorAction = { kind: 'retry' } | undefined

export type SessionStartSource = 'startup' | 'resume' | 'clear' | 'compact'

export interface Agent {
  readonly id: SessionId
  readonly options: AgentOptions
  readonly session: Session
  readonly inbox: Inbox
  readonly status: AgentStatus
  readonly ctx: Context

  cancel(cause: AgentCancelCause, options?: CancelOptions): void
  whenIdle(): Promise<void>
  runMaintenance<T>(task: (signal: AbortSignal) => Promise<T>): Promise<T>
  send(message: UserMessage, target: InboxTarget, wakeup: boolean): void
  followup(message: UserMessage): void
  steer(message: UserMessage): void
  inject(message: UserMessage): void
}
```

agent/* 事件（payload 类型照抄）:

```ts
declare module '@deepseek-ai/cordis' {
  interface Events {
    'agent/created'(this: Scoped<Agent>, payload: { agent: Agent }): void
    'agent/disposed'(this: Scoped<Agent>, payload: { agent: Agent }): void
    'agent/status'(this: Scoped<Agent>, payload: { agent: Agent; status: AgentStatus }): void
    'agent/inbox/inserted'(this: Scoped<Agent>, payload: { agent: Agent; message: UserMessage }): void
    'agent/inbox/claimed'(this: Scoped<Agent>, payload: { agent: Agent; message: UserMessage; turn: number }): void
    'agent/inbox/discarded'(this: Scoped<Agent>, payload: { agent: Agent; message: UserMessage }): void
    'agent/session-start'(this: Scoped<Agent>, payload: { agent: Agent; source: SessionStartSource }): void
    'agent/pre-step'(this: Scoped<Agent>, payload: { agent: Agent; messages: UserMessage[]; turn: number; step: number; signal: AbortSignal }, next: () => Promise<PreStepDecision>): Promise<PreStepDecision> // waterfall
    'agent/request'(this: Scoped<Agent>, payload: { agent: Agent; turn: number; step: number; signal: AbortSignal }, next: () => Promise<LlmCallConfig>): Promise<LlmCallConfig> // waterfall
    'agent/request-error'(this: Scoped<Agent>, payload: { agent: Agent; turn: number; step: number; provider: string; failure: LlmFailure; retryPolicy: ResolvedRetryPolicy | undefined; signal: AbortSignal }, next: () => Promise<RequestErrorAction>): Promise<RequestErrorAction> // waterfall
    'agent/turn-stopping'(this: Scoped<Agent>, payload: { agent: Agent; turn: number; signal: AbortSignal }): Promise<void> | void // serial
    'agent/error'(this: Scoped<Agent>, payload: { agent: Agent; turn: number; step: number; error: unknown }): void
  }
}
```

### `packages/core/agent/src/index.ts` — AgentRegistry / AgentHandle / CreateAgentOptions

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    agents: AgentRegistry
    agent?: Agent
  }
}

export interface AgentSetupCommit {
  commit(): void
}

export type AgentSetup = (
  agentCtx: Context,
) => AgentSetupCommit | Promise<AgentSetupCommit | void> | void

export interface CreateAgentOptions {
  readonly sessionId: SessionId
  readonly meta?: {
    readonly cwd?: string
    readonly parentSession?: SessionId
    readonly seedLength?: number
    readonly origin?: 'subagent'
    readonly delegationDepth?: number
    readonly agentPreset?: string
  }
  readonly seed?: readonly SessionEvent[]
  readonly agentOptions?: AgentOptions
  readonly signal?: AbortSignal
  readonly setup?: AgentSetup
}

export interface ResumeAgentOptions {
  readonly resumeSessionId: SessionId
  readonly agentOptions?: AgentOptions
  readonly signal?: AbortSignal
  readonly setup?: AgentSetup
}

export interface AgentHandle {
  agent: Agent
  dispose(): Promise<void>
}

export interface AgentFactory {
  createAgent(ownerCtx: Context, options: CreateAgentOptions): Promise<AgentHandle>
  resume(ownerCtx: Context, options: ResumeAgentOptions): Promise<AgentHandle>
}

export class AgentRegistry extends Service {
  constructor(ctx: Context)

  currentInitiator(): Agent | undefined
  requireInitiator(): Agent
  withInitiator<T>(agent: Agent, operation: () => T): T
  withoutInitiator<T>(operation: () => T): T
  setFactory(factory: AgentFactory): () => void
  async create(options: CreateAgentOptions): Promise<AgentHandle>
  async resume(options: ResumeAgentOptions): Promise<AgentHandle>
  register(agent: Agent): () => void
  enter(agent: Agent, owner: Agent | undefined): () => void
  announce(agent: Agent): void
  get(id: SessionId): Agent | undefined
  isOwnedBy(id: SessionId, owner: Agent): boolean
  list(): Agent[]
  roots(): Agent[]
}

export default AgentRegistry
```

---

## 4. `packages/core/system-prompt/src/index.ts`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    systemPrompt: SystemPrompt
  }

  interface Events {
    'system-prompt/assemble'(this: Scoped<SystemPrompt>, assembly: PromptAssembly, context: AssembleContext, next: () => Promise<PromptAssembly>): Promise<PromptAssembly> // waterfall
    'system-prompt/change'(): void
  }
}

export interface AssembleContext {
  scope?: ScopeKey
  signal?: AbortSignal
  agent?: Agent  // 由 dsh-agent 通过 declaration merging 增加
}

export interface PromptSection {
  readonly name: string
  readonly order: number
  readonly text: string | ((context: AssembleContext) => string)
  readonly complete?: boolean
}

export interface PromptContext {
  readonly name: string
  readonly order: number
  readonly text: string | ((context: AssembleContext) => string)
}

export interface AssembledSection {
  name: string
  text: string
}

export interface AssembledContext {
  name: string
  text: string
}

export interface ToolProviderResult {
  readonly schemas: readonly ToolSchema[]
  readonly knownNames?: readonly string[]
}

export interface PromptAssembly {
  sections: AssembledSection[]
  contexts: AssembledContext[]
  tools: ToolSchema[]
  variables: Record<string, string | undefined>
}

export const PERSONA_SECTION = 'deployment:persona'
export const PERSONA_ORDER = 0
export const TOOL_ORDER_REST = '<unlisted-tools>'

export interface Config {
  includeHarnessIdentity?: boolean
  includeRuntimeContext?: boolean
  persona?: string
  toolOrder?: string[]
}

export function renderPrompt(assembly: PromptAssembly): string
export function renderContextSnapshot(assembly: PromptAssembly): string
export function joinContextSections(sections: readonly ContextSnapshotSection[]): string
export function renderContextSections(assembly: PromptAssembly): ContextSnapshotSection[]

export class SystemPrompt extends Service {
  static Config: z<Config>

  constructor(ctx: Context, config: Config)

  section(section: PromptSection): () => void
  context(context: PromptContext): () => void
  suppressRuntimeContext(): () => void
  tools(provider: (context: AssembleContext) => ToolProviderResult): () => void
  variable(name: string, provider: (context: AssembleContext) => string | undefined): () => void
  async assemble(context: AssembleContext = {}): Promise<PromptAssembly>
}

export default SystemPrompt
```

---

## 5. `packages/llm/llm/src/index.ts`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    llm: LlmRuntime
  }

  interface Events {
    'llm/stream'(this: LlmRuntime, options: GenerateOptions, next: () => AsyncIterable<StreamChunk>): AsyncIterable<StreamChunk> // waterfall
  }
}

export interface LlmErrorOptions extends ErrorOptions {
  status?: number
  providerRetryAfterMs?: number
  requestId?: ProviderRequestId
}

export class LlmError extends HarnessError {
  readonly failure: LlmFailure
  constructor(message: string, code: string, options?: LlmErrorOptions)
}

export function assertUsableApiKey(raw: string, pkg: string, ref: string): string

export interface PreparedLlmCall {
  readonly config: LlmCallConfig
  readonly retryPolicy: ResolvedRetryPolicy
  readonly context?: LlmModelContext
  readonly adapterDefaults: LlmCallConfigAdapterDefaults
  stream(options: GenerateOptions): AsyncIterable<StreamChunk>
}

export abstract class LlmAdapter {
  providerInfo(provider: string): LlmProviderInfo
  providerRetryPolicy(_provider: string): ResolvedRetryPolicy | undefined
  listModels(_provider: string): Promise<readonly LlmModelInfo[]>
  resolveModel(
    provider: string,
    model: string,
    _signal?: AbortSignal,
  ): Promise<LlmResolvedModelInfo>
  abstract stream(options: GenerateOptions): AsyncIterable<StreamChunk>
}

export interface AdapterRegistrationHandle {
  (): void
  replace(providers: string[]): void
}

export interface DirectoryRegistrationHandle {
  (): void
  replace(entries: readonly LlmConfigurableProvider[]): void
}

export class LlmRuntime extends Service {
  constructor(ctx: Context)

  registerAdapter(providers: string[], adapter: LlmAdapter): AdapterRegistrationHandle
  listProviders(): LlmProviderInfo[]
  registerConfigurableProviders(entries: readonly LlmConfigurableProvider[]): DirectoryRegistrationHandle
  listConfigurableProviders(): LlmConfigurableProvider[]
  registerModelDiscovery(
    settingsNs: string,
    discover: (request: LlmModelDiscoveryRequest) => Promise<readonly LlmDiscoveredModel[]>,
  ): () => void
  async discoverModels(
    settingsNs: string,
    request: LlmModelDiscoveryRequest,
  ): Promise<LlmDiscoveredModel[]>
  providerRetryPolicy(provider: string): ResolvedRetryPolicy
  async listModels(provider: string): Promise<LlmModelInfo[]>
  async resolveModelInfo(
    provider: string,
    model: string,
    signal?: AbortSignal,
  ): Promise<LlmResolvedModelInfo>
  async resolveCallConfig(config: LlmCallConfig, signal?: AbortSignal): Promise<LlmCallConfig>
  async prepareCall(config: LlmCallConfig, signal?: AbortSignal): Promise<PreparedLlmCall>
  stream(options: GenerateOptions): AsyncIterable<StreamChunk>
}

export default LlmRuntime
```

`packages/llm/llm/src/types.ts` 中的 `StreamChunk` / `ToolSchema` / `GenerateOptions`:

```ts
export type StreamChunk =
  | { type: 'block-start'; index: number; blockType: ContentBlockType }
  | { type: 'text-delta'; index: number; text: string }
  | { type: 'reasoning-delta'; index: number; text: string }
  | { type: 'tool-call-delta'; index: number; id: CallId; name?: string; argumentsDelta: string }
  | { type: 'block-end'; index: number; block: ContentBlock }
  | { type: 'usage'; usage: TokenUsage }
  | {
    type: 'finish'
    reason: FinishReason
    replayState?: unknown
  }

export interface ToolSchema {
  name: string
  description: string
  parameters: Record<string, unknown>
}

export interface GenerateOptions {
  provider: string
  model: string
  reasoningEffort?: ReasoningEffortId
  messages: Message[]
  system?: string
  tools?: ToolSchema[]
  temperature?: number
  maxTokens?: number
  stop?: string[]
  signal?: AbortSignal
  sessionId?: Branded<'SessionId'>
  purpose?: 'compaction' | 'session-title'
}
```

---

## 6. `packages/interaction/commands/`

### `src/index.ts`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    commands: CommandRuntime
  }
}

export interface CommandInvocation {
  readonly commandId: CommandId
  readonly agent: Agent
  readonly rawInput: string
  readonly signal: AbortSignal
}

export interface CommandDefinition {
  readonly name: string
  readonly description: string
  readonly input?: CommandInputDescriptor
  readonly recordInput?: boolean
  readonly handler: (invocation: CommandInvocation) => CommandResult | Promise<CommandResult>
}

export interface ParsedCommand {
  readonly name: string
  readonly rawInput: string
}

export function parseCommand(line: string): ParsedCommand | undefined

export class CommandRuntime extends TypertRemoteService {
  constructor(ctx: Context)

  register(definition: CommandDefinition): () => void
  @Remote
  list(agent: Agent): readonly CommandDescriptor[]
  find(agent: Agent, name: string): CommandDefinition | undefined
  @Remote
  async execute(
    agent: Agent,
    line: string,
    signal: AbortSignal,
  ): Promise<CommandExecution | undefined>
}

export default CommandRuntime
```

### `src/types.ts`

```ts
export interface CommandInputDescriptor {
  readonly hint: string
}

export type CommandResult =
  | {
    readonly kind: 'success'
    readonly text?: string
    readonly sourceEventSeq?: number
  }
  | { readonly kind: 'error'; readonly text: string }

export interface CommandExecution {
  readonly commandId: CommandId
  readonly result: CommandResult
}

export interface CommandDescriptor {
  readonly name: string
  readonly description: string
  readonly input?: CommandInputDescriptor
}

export interface CommandSourceMap {
  user: { kind: 'user' }
}

export type CommandSource = CommandSourceMap[keyof CommandSourceMap]

declare module '@deepseek-ai/cordis' {
  interface Events {
    'commands/change'(): void
  }
}

declare module '@deepseek-ai/dsh-session/types' {
  interface SessionEventMap {
    'command/run': { commandId: CommandId; name: string; args?: string; source: CommandSource }
    'command/done': {
      commandId: CommandId
      kind: 'success' | 'error'
      text?: string
      sourceEventSeq?: number
    }
  }
}
```

---

## 7. `packages/core/session/src/index.ts`

Context/事件：

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    sessions: SessionStore
  }

  interface Events {
    'session/created'(this: Scoped<Session>, session: Session): void
    'session/disposed'(this: Scoped<Session>, session: Session): void
    'session/event'(this: Scoped<Session>, session: Session, event: SessionEvent): void
    'session/flush'(this: Scoped<Session>, session: Session): Promise<void> | void // parallel
  }
}
```

`Session`:

```ts
export class Session {
  get surface(): SessionSurface
  readonly header: SessionHeader
  get id(): SessionId
  readonly firstLiveSeq: number
  get events(): readonly SessionEvent[]
  get seq(): number

  static create(id: SessionId, seed?: readonly SessionEvent[], header?: SessionHeader): Session
  static fromRestore(id: SessionId, seed: readonly SessionEvent[], header: SessionHeader): Session

  append<T extends SessionEventType>(
    type: T,
    data: SessionEventMap[T],
    ...opts: T extends SurfaceEventType ? [opts: SurfaceIntent] : []
  ): SessionEvent<T>

  requestHeader(): EpochHeader | undefined
  requestContext(): RequestContext | undefined
  deriveMessages(): Message[]
  deriveEventMessage(event: SessionEvent): Message | null
}
```

`SessionStore`(`ctx.sessions`):

```ts
export type SessionForkSource = Session | SessionId

export type SessionForkErrorCode =
  | 'SESSION_NOT_FOUND'
  | 'SESSION_NOT_LIVE'
  | 'SESSION_ALREADY_EXISTS'
  | 'INVALID_BOUNDARY'
  | 'OPEN_TURN'

export class SessionForkError extends Error {
  constructor(message: string, public readonly code: SessionForkErrorCode)
}

export class SessionStore extends Service {
  constructor(ctx: Context)

  create(id?: SessionId, options?: CreateSessionOptions): Session
  prepare(id?: SessionId, options?: PrepareSessionOptions): Session
  enter(session: Session): () => void
  announce(session: Session): void
  async flush(session: Session): Promise<boolean>
  get(id: SessionId): Session | undefined
  list(): Session[]
  fork(source: SessionForkSource, boundary?: number, childSessionId?: SessionId): Session
}
```

session 相关类型（`src/types.ts`):

```ts
export const SESSION_FORMAT_VERSION = 0

export interface SessionHeader {
  readonly version: number
  readonly id: SessionId
  readonly createdAt: number
  readonly cwd?: string
  readonly parentSession?: SessionId
  readonly seedLength?: number
  readonly origin?: 'subagent'
  readonly delegationDepth?: number
  readonly agentPreset?: string
}

export interface CreateSessionOptions {
  readonly seed?: readonly SessionEvent[]
  readonly meta?: {
    readonly cwd?: string
    readonly parentSession?: SessionId
    readonly createdAt?: number
    readonly seedLength?: number
    readonly origin?: 'subagent'
    readonly delegationDepth?: number
    readonly agentPreset?: string
  }
}

export interface RestoredSessionOptions {
  readonly seed: SessionEvent[]
  readonly meta: SessionHeader
  readonly seedSource: 'persistence'
}

export type PrepareSessionOptions =
  | (CreateSessionOptions & { readonly seedSource?: undefined })
  | RestoredSessionOptions

export interface SessionEventMap {
  'turn/start': { turn: number }
  'turn/end': { turn: number; reason: TurnEndReason }
  'step/start': { turn: number; step: number }
  'step/end': { turn: number; step: number }
  'user/message': UserMessage
  'assistant/chunk': { turn: number; step: number; chunk: StreamChunk }
  'assistant/message': { turn: number; step: number; message: AssistantMessage; usage?: TokenUsage }
  'tool/call': { turn: number; step: number; callId: CallId; name: string; arguments: string }
  'tool/result': {
    turn: number
    step: number
    message: ToolResultMessage
    error?: { name: string; code: string }
    meta?: JsonValue
  }
  'todo/write': { todos: TodoItem[] }
  'request/header': { header: EpochHeader; reason: RequestHeaderReason }
  'request/context': RequestContext
  'session/end-seed': Record<string, never>
}

export type SessionEventType = keyof SessionEventMap

export type SessionEvent<T extends SessionEventType = SessionEventType> = {
  [K in SessionEventType]: {
    type: K
    seq: number
    time: number
    data: SessionEventMap[K]
    ignorable?: true
  } & (K extends SurfaceEventType ? {
    sourceEventSeqs?: number[]
    surfaceOp?: SurfaceOp
  } : object)
}[T]
```

其它导出（session/index.ts 顶层）:

```ts
export function adoptSessionEvent<T extends SessionEvent>(event: T): T
export function snapshotSessionEvent<T extends SessionEvent>(event: T): T
export { SessionPreparation } from './preparation.ts'
export type { SessionPreparationOptions } from './preparation.ts'
export type { AssistantMessage, ToolResultMessage, UserMessage } from '@deepseek-ai/dsh-llm'
export { isJsonValue, snapshotJsonValue } from './json.ts'
export type { JsonValue } from './json.ts'
```

---

## 8. `packages/skill/skill/src/index.ts`

```ts
export function isSkillName(name: string): boolean

export type SkillSource = 'project-dsh' | 'project-agents' | 'runtime' | 'user-dsh' | 'user-agents' | 'custom' | 'bundled' | (string & {})

export type SkillResourceBase =
  | { readonly kind: 'directory'; readonly path: string }
  | { readonly kind: 'url'; readonly url: string }
  | { readonly kind: 'opaque'; readonly description: string }

export interface SkillInvocationPolicy {
  readonly modelInvocable: boolean
  readonly userInvocable: boolean
}

export interface SkillSummary {
  readonly name: string
  readonly description: string
  readonly whenToUse?: string
  readonly invocation: SkillInvocationPolicy
  readonly source: SkillSource
  readonly provider: string
  readonly resourceBase?: SkillResourceBase
}

export interface SkillCandidate extends SkillSummary {
  readonly rank: number
  readonly locator: unknown
  readonly path?: string
  readonly metadata?: Readonly<Record<string, unknown>>
}

export interface SkillDefinition extends SkillSummary {
  readonly content: string
  readonly path?: string
  readonly metadata?: Readonly<Record<string, unknown>>
}

export type SkillRegistration = Omit<SkillDefinition, 'invocation' | 'provider'> & {
  readonly invocation?: SkillInvocationPolicy
  readonly provider?: string
}

export interface SkillLookupOptions {
  readonly cwd?: string | undefined
  readonly signal?: AbortSignal | undefined
}

export interface SkillViewOptions extends SkillLookupOptions {
  readonly scope?: ScopeKey | undefined
}

export function isModelInvocable(skill: Pick<SkillSummary, 'invocation'>): boolean
export function isUserInvocable(skill: Pick<SkillSummary, 'invocation'>): boolean

export interface SkillInvocationSource {
  readonly kind: 'skill-invocation'
  readonly name: string
  readonly form: 'instructions'
}

export function renderSkillContent(skill: Pick<SkillDefinition, 'name' | 'provider' | 'resourceBase' | 'content'>): string
export function escapeText(value: string): string

export interface SkillCatalogSnapshot {
  readonly skills: SkillSummary[]
  readonly complete: boolean
}

export interface SkillProviderObservation {
  readonly candidates: readonly SkillCandidate[]
  readonly complete: boolean
}

export interface SkillProvider {
  readonly name: string
  readonly list: (options: SkillLookupOptions) => Promise<readonly SkillCandidate[] | SkillProviderObservation>
  readonly get: (candidate: SkillCandidate, options: SkillLookupOptions) => Promise<SkillDefinition | undefined>
}

export interface SkillProviderControl {
  readonly signal: AbortSignal
  readonly invalidate: () => void
}

export interface Config {
  readonly collectCacheMaxEntries?: number
}

declare module '@deepseek-ai/cordis' {
  interface Context {
    skills: SkillRegistry
  }

  interface Events {
    'skills/change'(): void
  }
}

export class SkillRegistry extends Service {
  static Config: Schema<Config>

  constructor(ctx: Context, config: Config = {})

  registerProvider(create: (control: SkillProviderControl) => SkillProvider): () => void
  register(skill: SkillRegistration): () => void
  async list(options: SkillViewOptions = {}): Promise<SkillSummary[]>
  async snapshot(options: SkillViewOptions = {}): Promise<SkillCatalogSnapshot>
  async get(name: string, options: SkillViewOptions = {}): Promise<SkillDefinition | undefined>
}
```

---

## 9. Capability seam Provider 接口

### `ctx.fs` — `packages/fs/fs/src/index.ts`:`FileSystem`(abstract class)

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    fs: FileSystem
  }

  interface Events {
    'fs/write-intent'(target: FsTarget, actor: object | undefined, next: () => FsWriteIntent | undefined | Promise<FsWriteIntent | undefined>): Promise<FsWriteIntent | undefined> // waterfall
    'fs/edit-intent'(target: FsTarget, actor: object | undefined, next: () => { version: FsVersion } | undefined | Promise<{ version: FsVersion } | undefined>): Promise<{ version: FsVersion } | undefined> // waterfall
    'fs/observed'(target: FsTarget, observation: FsObservation, actor: object | undefined): void // emit
  }
}

export abstract class FileSystem extends Service {
  constructor(ctx: Context)

  get sandboxMode(): SandboxMode | undefined

  abstract resolve(path: string, opts?: { cwd?: string; signal?: AbortSignal }): Promise<FsTarget>
  abstract processPath(target: FsTarget): string
  abstract fileUrl(target: FsTarget): string
  abstract contains(parent: FsTarget, child: FsTarget): boolean
  abstract stat(target: FsTarget, signal?: AbortSignal): Promise<FsInfo | undefined>
  abstract lstat(path: string, opts?: { cwd?: string }, signal?: AbortSignal): Promise<FsPathInfo | undefined>
  abstract readText(target: FsTarget, signal?: AbortSignal): Promise<string>
  abstract streamText(target: FsTarget, signal?: AbortSignal): Promise<AsyncIterable<string>>
  abstract readBytes(target: FsTarget, signal: AbortSignal | undefined, maxBytes: number): Promise<Uint8Array>
  abstract listDir(target: FsTarget, signal?: AbortSignal): Promise<FsDirEntry[]>
  abstract writeText(
    target: FsTarget,
    content: string,
    expected?: FsWriteIntent,
    signal?: AbortSignal,
    sandboxPolicy?: SandboxExecutionPolicy,
  ): Promise<FsWriteOutcome>
  abstract editText(
    target: FsTarget,
    edit: FsEditRequest,
    expected?: { version: FsVersion },
    signal?: AbortSignal,
    sandboxPolicy?: SandboxExecutionPolicy,
  ): Promise<FsEditOutcome>
}
```

### `ctx.shell` — `packages/shell/shell/src/index.ts`:`ShellExecutor`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    shell: ShellExecutor
  }
}

export abstract class ShellExecutor extends Service {
  constructor(ctx: Context)

  get sandboxMode(): SandboxMode | undefined

  abstract resolve(request: ShellExecRequest): ShellExecSpec
  abstract run(spec: ShellExecSpec): Promise<ShellRunResult>
  abstract start(spec: ShellExecSpec): ShellProcess
}
```

配套类型（`shell/shell/src/types.ts`):

```ts
export interface ShellExecRequest {
  command: string
  workdir?: string | undefined
  timeoutMs?: number | undefined
  stdoutMaxBytes?: number | undefined
  signal?: AbortSignal | undefined
  stdin?: string | undefined
  env?: Record<string, string> | undefined
  dshEnv?: DshEnvironment | undefined
  sandboxPolicy?: SandboxExecutionPolicy | undefined
}

export interface ShellExecSpec {
  command: string
  workdir: string
  timeoutMs: number
  stdoutMaxBytes: number
  signal?: AbortSignal | undefined
  stdin?: string | undefined
  env?: Record<string, string> | undefined
  dshEnv?: DshEnvironment | undefined
  sandboxPolicy: SandboxExecutionPolicy | undefined
}

export interface ShellRunResult {
  exitCode: number | null
  signal: NodeJS.Signals | null
  timedOut: boolean
  aborted: boolean
  timeoutMs: number
  stdout: CollectedOutput
  stderr: CollectedOutput
  sandbox?: ShellSandboxInfo
}

export type ShellProcessStatus = 'running' | 'completed' | 'killed'

export interface ShellProcessRead {
  delta: string
  lossy: boolean
  stdoutSpillPath?: string
  stderrSpillPath?: string
}

export interface ShellProcess {
  status: ShellProcessStatus
  exitCode: number | null
  signal: NodeJS.Signals | null
  readonly done: Promise<void>
  sandbox?: ShellSandboxInfo
  readOutput(): ShellProcessRead
  kill(): boolean
}
```

### `ctx.subprocess` — `packages/subprocess/subprocess/src/index.ts`:`SubprocessRuntime`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    subprocess: SubprocessRuntime
  }
}

export abstract class SubprocessRuntime extends Service {
  constructor(ctx: Context)

  abstract resolveExecutable(
    command: string,
    env?: Readonly<Record<string, string>>,
    signal?: AbortSignal,
  ): Promise<string>

  abstract spawn(spec: SubprocessSpawnSpec): SubprocessHandle

  abstract spawnTerminal(spec: SubprocessTerminalSpawnSpec): Promise<SubprocessTerminalHandle>
}
```

配套类型（`subprocess/subprocess/src/types.ts`):

```ts
export interface SubprocessSpawnSpec {
  argv: readonly string[]
  cwd: string
  stdio: SubprocessStdio
  graceMs: number
  signal?: AbortSignal | undefined
  env?: NodeJS.ProcessEnv | undefined
}

export interface SubprocessHandle {
  readonly pid: number
  readonly stdin: Writable | undefined
  readonly stdout: Readable | undefined
  readonly stderr: Readable | undefined
  readonly collected: SubprocessCollectedOutputs
  readonly done: Promise<SubprocessOutcome>
  terminate(): void
  waitForExit(signal?: AbortSignal): Promise<boolean>
}

export type SubprocessTerminalSignal = 'SIGINT' | 'SIGTERM' | 'SIGKILL' | 'SIGTSTP' | 'SIGHUP'

export interface SubprocessTerminalSpawnSpec {
  argv: readonly string[]
  cwd: string
  env?: Record<string, string> | undefined
  rows: number
  cols: number
  graceMs: number
  signal?: AbortSignal | undefined
}

export interface SubprocessTerminalForeground {
  processGroupId: number
  inputWaiting: boolean
}

export interface SubprocessTerminalHandle {
  readonly pid: number
  readonly output: Readable
  readonly done: Promise<SubprocessOutcome>
  write(data: string): Promise<void>
  inspectForeground(): Promise<SubprocessTerminalForeground | undefined>
  signalForeground(signal: SubprocessTerminalSignal): Promise<number>
  // + idempotent terminate-and-await-quiescence method (见源码 types.ts)
}
```

### `ctx.subagents` — `packages/subagent/subagent/src/index.ts`:`SubagentRuntime`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    subagents: SubagentRuntime
  }

  interface Events {
    'subagent/provider-added'(provider: SubagentProvider): void
    'subagent/provider-removed'(name: string): void
    'subagent/start'(this: Scoped<SubagentRuntime>, info: SubagentRunInfo): void
    'subagent/end'(this: Scoped<SubagentRuntime>, info: SubagentRunEndInfo): void
  }
}

export class SubagentRuntime extends Service {
  constructor(ctx: Context)

  async startContinuable(spec: ContinuableStartSpec): Promise<ContinuableStart>
  async followup(
    parent: Agent,
    childId: SessionId,
    content: ContentBlock[],
    options: SubagentFollowupOptions,
  ): Promise<MessageId>
  interrupt(targetSessionId: SessionId, authority: SubagentInterruptAuthority): void
  async reportFrom(
    child: Agent,
    content: ContentBlock[],
    options: SubagentReportOptions,
  ): Promise<MessageId>
  registerContinuableSetup(contribution: ContinuableSetupContribution): () => void
  async drainContinuableDescendants(parents: readonly Agent[]): Promise<void>
  listChildren(parentSessionId: SessionId, signal?: AbortSignal): Promise<SubagentListEntry[]>
  listDescendants(rootSessionId: SessionId, signal?: AbortSignal): Promise<SubagentDescendantListEntry[]>
  registerProvider(provider: SubagentProvider): () => void
  getProvider(name: string): SubagentProvider | undefined
  list(): string[]
  async start(name: string, request: SubagentStartRequest): Promise<SubagentRun>
}
```

`SubagentProvider`(`subagent/subagent/src/types.ts`):

```ts
export interface SubagentProvider {
  readonly name: string
  readonly capabilities: SubagentCapabilities
  readonly inheritsParentContext: boolean
  start(request: ResolvedSubagentStartRequest): Promise<SubagentRun>
  prepareContinuable?(request: ContinuableCreateRequest): Promise<ContinuableCreateSpec>
}
```

### `ctx.web` — `packages/web/web/src/index.ts`:`WebRuntime`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    web: WebRuntime
  }
}

export interface WebRuntimeConfig {
  // searchProvider / fetchProvider（schema: z.string()）
}

export class WebRuntime extends Service {
  static Config: z<WebRuntimeConfig>

  constructor(ctx: Context, config: WebRuntimeConfig = {})

  registerSearchProvider(provider: WebSearchProvider): () => void
  registerFetchProvider(provider: WebFetchProvider): () => void
  async search(request: WebSearchRequest, signal?: AbortSignal): Promise<WebSearchResult>
  async fetch(request: WebFetchRequest, signal?: AbortSignal): Promise<WebFetchResult>
}
```

Provider(`web/web/src/types.ts`):

```ts
export interface WebSearchProvider {
  readonly id: string
  available(): boolean
  search(request: WebSearchRequest, signal?: AbortSignal): Promise<WebSearchResult>
}

export interface WebFetchProvider {
  readonly id: string
  available(): boolean
  fetch(request: WebFetchRequest, signal?: AbortSignal): Promise<WebFetchResult>
}

export class WebError extends HarnessError {}
```

### `ctx.jobs` — `packages/jobs/jobs/src/index.ts`:`JobRegistry`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    jobs: JobRegistry
  }
}

export abstract class JobRegistry extends Service {
  constructor(ctx: Context)

  abstract start(spec: JobStart): JobId
  abstract list(caller?: Agent): JobSnapshot[]
  abstract get(id: JobId, caller?: Agent): JobSnapshot
  abstract read(id: JobId, caller?: Agent): JobRead
  abstract kill(id: JobId, caller?: Agent, reason?: string): 'requested' | 'already-finished'
  abstract wait(id: JobId, timeoutMs: number, caller?: Agent, signal?: AbortSignal): Promise<JobSnapshot>
  abstract onJobDone(listener: JobDoneListener): () => void
  abstract onJobsChanged(listener: JobsChangedListener): () => void
  abstract attachController(name: string): () => void
}
```

### `ctx.compaction` — `packages/compaction/compaction/src/index.ts`:`CompactionEngine`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    compaction: CompactionEngine
  }
}

export abstract class CompactionEngine extends Service {
  constructor(ctx: Context)

  abstract compactIfNeeded(
    agent: CompactionAgentContext,
    trigger: CompactionTrigger,
    signal: AbortSignal,
  ): Promise<CompactionResult | null>

  abstract compactNow(
    agent: ManualCompactAgentContext,
    signal: AbortSignal,
    sourceCommandId?: CommandId,
  ): Promise<CompactionResult | null>

  abstract compactRegion(
    start: number,
    end: number,
    agent: CompactionAgentContext,
    signal?: AbortSignal,
  ): Promise<CompactionResult>
}
```

### `ctx.approval` — `packages/interaction/user-approval/src/index.ts`:`ApprovalService`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    approval: ApprovalService
  }

  interface Events {
    'approval/request'(this: Scoped<ApprovalService>, req: ApprovalRequest, next: () => Promise<ApprovalOutcome>): Promise<ApprovalOutcome> // waterfall
  }
}

declare module '@deepseek-ai/dsh-session/types' {
  interface SessionEventMap {
    'approval/asked': {
      id: ApprovalRequestId
      toolName: string
      callId?: CallId
      reason?: string
    }
    'approval/decided': {
      id: ApprovalRequestId
      outcome: ApprovalOutcome
    }
    'approval/policy': {
      policy: ApprovalPolicy
      source?: 'delegation'
    }
  }
}

export type ApprovalPolicy = 'ask' | 'never'
export const APPROVAL_POLICIES: readonly ApprovalPolicy[] = ['ask', 'never']

export function effectiveApprovalPolicy(events: readonly SessionEvent[]): ApprovalPolicy | undefined
export function setApprovalPolicy(session: Session, policy: ApprovalPolicy): void

export interface ApprovalRequest {
  readonly agent: Agent
  readonly toolName: string
  readonly callId?: CallId
  readonly reason?: string
  readonly signal?: AbortSignal
}

export interface Config {
  readonly policy?: ApprovalPolicy
}

export class ApprovalService extends Service {
  static Config: z<Config>

  constructor(ctx: Context, public config: Config)

  setPolicy(agent: Agent, policy: ApprovalPolicy): void
  async request(req: ApprovalRequest): Promise<ApprovalOutcome>
  overrideOf(session: Session): ApprovalPolicy | undefined
}
```

(`ApprovalOutcome` 来自 `./types.ts`:`'allowed-once' | 'rejected' | 'cancelled' | 'unavailable'` —— 由 `OUTCOMES` 常量列出。)

### `ctx.settings` — `packages/settings/settings/src/index.ts`:`SettingsProvider`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    settings: SettingsProvider
  }
}

export abstract class SettingsProvider extends Service {
  constructor(ctx: Context)

  async* [Service.init](): AsyncGenerator<() => Promise<void> | void, void, void>

  abstract readonly writable: boolean

  get documentPath(): string | undefined
  prepareDocument(): Promise<string | undefined>

  protected abstract load(): Promise<Record<string, unknown>>
  protected abstract persist(ns: SettingsNamespace, section: Record<string, unknown>): Promise<void>

  register<T>(ns: SettingsNamespace, schema: z<T>, options?: SettingsRegisterOptions<T>): SettingsScope<T>
  describe(options?: SettingsDescribeOptions): SettingsDescriptor[]
  get(ns: SettingsNamespace): unknown
  async update(ns: SettingsNamespace, patch: object, expectedRevision?: number): Promise<void>
  async replace(ns: SettingsNamespace, section: object, expectedRevision?: number): Promise<void>
  async mutate(ns: SettingsNamespace, ops: readonly SettingsPathOp[], expectedRevision?: number): Promise<void>

  protected publish(doc: Record<string, unknown>, source: SettingsUpdateSource = 'provider'): void
}
```

（`register()` 返回的 `SettingsScope<T>` 形状：`{ get: () => T; watch: (callback) => () => void; update: (patch) => Promise<void>; replace: (section) => Promise<void> }`，见 `register` 实现。事件：`settings/updated`、`settings/document-updated`，由内部 commit 路径 emit。)

### `ctx.credentials` — `packages/credentials/credentials/src/index.ts`:`CredentialProvider`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    credentials: CredentialProvider
  }
}

export abstract class CredentialProvider extends Service {
  constructor(ctx: Context)

  abstract resolve(ref: CredentialRef): Promise<ResolvedCredential | undefined>
  abstract describe(ref: CredentialRef): Promise<CredentialInfo>
  abstract set(ref: CredentialRef, value: string): Promise<void>
  abstract unset(ref: CredentialRef): Promise<void>

  protected notifyUpdated(ref: CredentialRef): void
}
```

（事件：`credentials/updated`(ref），由 provider 提交后 fan-out。)

### `ctx.storage` — `packages/storage/storage/src/index.ts`:`Storage`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    storage: Storage
  }
}

export interface StorageForms {}  // declaration-merged by form owners

export class Storage extends Service {
  readonly backend: BackendRegistry = new BackendRegistry()

  constructor(ctx: Context)

  mount<K extends keyof StorageForms>(form: K, facility: StorageForms[K]): () => void
  form<K extends keyof StorageForms>(form: K): StorageForms[K]
  get domain(): StorageForms extends { domain: infer D } ? D : never
}
```

`BackendRegistry`(`storage/storage/src/registry.ts`):

```ts
export class BackendRegistry {
  register(name: string, backend: StorageBackend): () => void
  get(name: string): StorageBackend
  names(): string[]
}
```

### `ctx.terminals` — `packages/terminal/terminal/src/index.ts`:`TerminalSessionService`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    terminals: TerminalSessionService
  }
}

export class TerminalSessionService extends Service {
  constructor(ctx: Context)

  registerBackend(backend: TerminalBackend): () => void
  listBackends(): string[]
  async spawn(owner: Agent, request: TerminalSpawnRequest, signal?: AbortSignal): Promise<TerminalSpawnResult>
  hasOwnerActivity(owner: Agent): boolean
  startSend(owner: Agent, id: TerminalSessionId, request: TerminalSendRequest): TerminalSendOperation
  read(owner: Agent, id: TerminalSessionId, request: TerminalReadRequest = {}): TerminalReadResult
  signal(owner: Agent, id: TerminalSessionId, signal: TerminalSignal): Promise<TerminalSignalResult>
  async kill(owner: Agent, id: TerminalSessionId, reason: string = 'model request'): Promise<boolean>
  list(owner: Agent): TerminalSessionSnapshot[]
}
```

Provider(`terminal/terminal/src/types.ts`):

```ts
export interface TerminalBackendSession {
  readonly motd: string
  readonly pid?: number
  startSend(request: TerminalSendRequest): TerminalSendOperation
  read(request: TerminalReadRequest): TerminalReadResult
  signal(signal: TerminalSignal): Promise<TerminalSignalResult>
  status(): TerminalSessionStatus
  close(reason: string): Promise<void>
}

export interface TerminalBackend {
  readonly type: string
  spawn(spec: TerminalBackendSpawnSpec): Promise<TerminalBackendSession>
}

export interface TerminalSpawnResult extends TerminalSessionSnapshot {
  motd: string
}
```

### `ctx.lsp` — `packages/lsp/lsp/src/types.ts`:`LspService` / `LspProvider`

```ts
// index.ts: interface Context { lsp: LspService };实现类:
// export class Lsp extends Service implements LspService

export interface LspService {
  registerProvider(provider: LspProvider): () => void
  query(request: LspQueryRequest, signal?: AbortSignal): Promise<LspQueryResult>
}

export interface LspProvider {
  readonly id: LspProviderId
  readonly extensionToLanguage: Readonly<Record<string, string>>
  query(request: LspProviderQuery, signal?: AbortSignal): Promise<LspQueryResult>
}

export interface LspProviderQuery extends LspQueryRequest {
  readonly languageId: string
}

export type LspQueryResult =
  | { readonly kind: 'locations'; readonly locations: readonly LspLocation[]; readonly resolvedWorkspaceUri: string }
  | { readonly kind: 'hover'; readonly hover: LspHover | null }
```

### `ctx.workflowEngine` — `packages/workflow/workflow/src/index.ts`:`WorkflowEngine`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    workflowEngine: WorkflowEngine
  }
  interface Events {
    // workflow/* lifecycle events(见该文件 Events 声明)
  }
}

export abstract class WorkflowEngine extends Service {
  constructor(ctx: Context)

  abstract start(request: WorkflowStartRequest): WorkflowRun

  protected emitWorkflowEvent(name: WorkflowEventName, ...args: unknown[]): void
}
```

### `ctx.codeRuntime` — `packages/code-runtime/code-runtime/src/index.ts`:`CodeRuntime`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    codeRuntime: CodeRuntime
  }
}

export abstract class CodeRuntime extends Service {
  abstract readonly language: string
  abstract readonly isolation: string

  constructor(ctx: Context)

  abstract run(request: CodeRunRequest): Promise<CodeRunResult>
}
```

### `ctx.goals` — `packages/goal/goal/src/index.ts`:`GoalService`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    goals: GoalService
  }
}

export class GoalService extends TypertRemoteService {
  static inject = ['agents']
  static Config: z<Config>  // { defaultMaxGoalRounds: z.number().default(256) }

  constructor(ctx: Context, config: Config = {})

  get(agent: Agent): GoalView | undefined
  disarm(agent: Agent): GoalView | undefined
  create(agent: Agent, request: CreateGoalRequest): GoalView
  @Remote('edit')
  edit(agent: Agent, ref: GoalRef, request: EditGoalRequest): GoalView
  @Remote('pause')
  pause(agent: Agent, ref: GoalRef): GoalView
  @Remote('resume')
  resume(agent: Agent, ref: GoalRef): GoalView
  @Remote('complete')
  complete(agent: Agent, ref: GoalRef): GoalView
  block(agent: Agent, ref: GoalRef, reason: GoalBlockReason): GoalView
  @Remote('clear')
  clear(agent: Agent, ref: GoalRef): GoalRef
}
```

### `ctx.planMode` — `packages/plan/plan-mode/src/index.ts`:`PlanModeController`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    planMode: PlanModeController
  }
}

export class PlanModeController extends Service {
  static inject = ['tools', 'systemPrompt']

  constructor(ctx: Context, config: PlanModeConfig = { section: '' })

  get(agent: Agent): { active: boolean; pending?: boolean }
  set(agent: Agent, active: boolean): 'committed' | 'queued' | 'cancelled' | 'noop'
}
```

（该服务还注册 `/plan` command 与 `exit_plan_mode` 工具，并写 `plan/mode` session 事件。)

---

## 10. `packages/extensions/` — 自修改 API

### `packages/extensions/cordis-host-runner/src/index.ts`:`ctx.dynamicCordisRunner`

```ts
declare module '@deepseek-ai/cordis' {
  interface Context {
    dynamicCordisRunner: DynamicCordisRunnerService
  }
}

export function CordisDynamicPluginId(id: string): CordisDynamicPluginId
export function CordisDynamicPackageId(id: string): CordisDynamicPackageId
export function CordisDynamicPluginRunId(id: string): CordisDynamicPluginRunId
export function ApprovalRequestId(id: string): ApprovalRequestId

export interface Config {
  vmTimeoutMs?: number
}

export interface DynamicCordisSnapshotRow {
  pluginId: CordisDynamicPluginId
  currentPackageId?: CordisDynamicPackageId
  nextPackageId?: CordisDynamicPackageId
  packages: Array<{
    packageId: CordisDynamicPackageId
    name: string
    purpose: string
    hasHostHalf: boolean
    hasClientHalf: boolean
  }>
  activeRun?: {
    pluginRunId: CordisDynamicPluginRunId
    packageId: CordisDynamicPackageId
    fiber?: Fiber
    handlers: string[]
    renderFailure?: DynamicCordisRenderFailure
  }
  latestRun?: DynamicCordisRunAttempt
}

export class DynamicCordisRunnerService extends TypertRemoteService {
  static inject = ['tools']
  static Config: z<Config>

  constructor(ctx: Context, config: Config)

  define(request: DynamicCordisDefineRequest): DynamicCordisDefineReceipt
  async undefine(agent: Agent, pluginId: CordisDynamicPluginId): Promise<DynamicCordisUndefineReceipt>
  @Remote('undefineFromPanel')
  async undefineFromPanel(agent: Agent, pluginId: CordisDynamicPluginId): Promise<DynamicCordisUndefineReceipt>
  async run(
    agent: Agent,
    pluginId: CordisDynamicPluginId,
    packageId: CordisDynamicPackageId,
    mode: CordisDynamicRunMode,
    signal?: AbortSignal,
  ): Promise<DynamicCordisRunResponse>
  @Remote('runHostHalf')
  async runHostHalf(
    agent: Agent,
    pluginId: CordisDynamicPluginId,
    packageId: CordisDynamicPackageId,
    mode: CordisDynamicRunMode,
    requestId: ApprovalRequestId | null,
    approveFutureVersions: boolean,
  ): Promise<DynamicCordisHostHalfResult>
  @Remote('getClientCode')
  getClientCode(
    agent: Agent,
    pluginId: CordisDynamicPluginId,
    pluginRunId: CordisDynamicPluginRunId,
  ): DynamicCordisClientSource
  @Remote('resolveRequestRun')
  async resolveRequestRun(
    requestId: ApprovalRequestId,
    resolution: DynamicCordisRunResolution,
  ): Promise<DynamicCordisResolveAck>
  @Remote('settleUserRun')
  async settleUserRun(
    agent: Agent,
    pluginId: CordisDynamicPluginId,
    resolution: DynamicCordisRunResolution,
  ): Promise<DynamicCordisRunResponse>
  async stop(agent: Agent, pluginId: CordisDynamicPluginId): Promise<DynamicCordisStopResponse>
  @Remote('stopFromPanel')
  async stopFromPanel(agent: Agent, pluginId: CordisDynamicPluginId): Promise<DynamicCordisStopResponse>
  @Remote('syncInspectManifest')
  syncInspectManifest(providers: readonly CordisInspectProviderManifest[]): null
  @Remote('resolveInspectQuery')
  resolveInspectQuery(
    agent: Agent,
    requestId: CordisInspectRequestId,
    resolution: CordisInspectQueryResolution,
  ): CordisInspectResolveAck
  @Remote('inventory')
  inventory(): DynamicCordisInventoryRow[]
  snapshot(agent: Agent): DynamicCordisSnapshotRow[]
  reference(agent: Agent, pluginId: CordisDynamicPluginId): DynamicCordisReference | undefined
  listPlugins(agent: Agent): DynamicCordisPluginInspection[]
  inspectPlugin(agent: Agent, pluginId: CordisDynamicPluginId): DynamicCordisPluginInspection
  inspectPackage(
    agent: Agent,
    pluginId: CordisDynamicPluginId,
    packageId: CordisDynamicPackageId,
  ): DynamicCordisPackageInspection
  @Remote('reportRenderFailure')
  async reportRenderFailure(
    agent: Agent,
    pluginId: CordisDynamicPluginId,
    pluginRunId: CordisDynamicPluginRunId,
    failure: DynamicCordisRenderFailure,
  ): Promise<null>
  @Remote('reportClientGuardFailure')
  async reportClientGuardFailure(
    agent: Agent,
    pluginId: CordisDynamicPluginId,
    pluginRunId: CordisDynamicPluginRunId,
    failure: CordisErrorDetails,
  ): Promise<null>
  @Remote('invoke')
  async invoke(
    pluginId: CordisDynamicPluginId,
    pluginRunId: CordisDynamicPluginRunId,
    method: string,
    args: JsonValue,
  ): Promise<DynamicCordisInvokeResult>
}
```

（相关 emit 事件：`'cordis/request-run'`、`'cordis/dynamic-package'`、`'cordis/inspect-query'`、`'cordis/inspect-query-resolved'`，见同文件/inspect-registry.ts。)

### `packages/extensions/cordis-host-runner/src/inspect-registry.ts`:`ctx.cordisInspect`

```ts
export interface HostCordisInspectQueryContext {
  signal: AbortSignal
  agent: Agent
}

export interface HostCordisInspectProviderRegistration {
  manifest: CordisInspectProviderManifest
  query(method: string, input: JsonValue | undefined, context: HostCordisInspectQueryContext): Promise<JsonValue>
}

declare module '@deepseek-ai/cordis' {
  interface Context {
    cordisInspect: CordisInspectRegistryService
  }
}

export class CordisInspectRegistryService extends Service {
  constructor(ctx: Context)

  register(registration: HostCordisInspectProviderRegistration): () => void
  syncClientManifest(providers: readonly CordisInspectProviderManifest[]): void
  list(): CordisInspectProviderView[]
  async query(
    platform: CordisInspectPlatform,
    providerId: string,
    methodName: string,
    input: JsonValue | undefined,
    agent: Agent,
    signal: AbortSignal,
  ): Promise<JsonValue>
  resolveClientQuery(
    agent: Agent,
    requestId: CordisInspectRequestId,
    resolution: CordisInspectQueryResolution,
  ): CordisInspectResolveAck
}
```

---

## 11. UI 扩展 — `ConversationNodeDefinition`

`packages/client/runtime/src/client/contract/conversation.ts`:

```ts
export interface ConversationNodeContext<State = unknown> {
  readonly key: string
  readonly kind: string
  readonly id: string
  readonly matches: readonly ConversationMatch[]
  readonly start: ConversationMatch | undefined
  readonly state: State | undefined
  readonly current: ReadonlyMap<string, ConversationViewNode | null>
}

export interface ConversationPreviousContext<State = unknown> {
  readonly key: string
  readonly kind: string
  readonly id: string
  readonly startSeq: number
  readonly state: Readonly<State>
  readonly matches: readonly ConversationMatch[]
}

export interface ConversationContextReader {
  previous<State>(kind: string): ConversationPreviousContext<State> | undefined
}

export type ConversationPublication = 'none' | 'animation-frame' | 'immediate'
export type ConversationLocationDataScope = 'step' | 'turn'

export interface ConversationNodeDefinition<State = unknown> {
  readonly kind: string
  readonly target?: string
  match(event: SessionEvent): ConversationMatchResult | null
  start(
    context: ConversationNodeContext<State>,
    match: ConversationMatch,
    reader: ConversationContextReader,
  ): State
  update(
    context: ConversationNodeContext<State> & { readonly state: State },
    match: ConversationMatch,
  ): State
  publication?(match: ConversationMatch): ConversationPublication
  buildLocationData?(
    context: ConversationNodeContext<State>,
    scope: ConversationLocationDataScope,
  ): ConversationLocationData | null
  buildViewNode?(context: ConversationNodeContext<State>): ConversationViewNode | null
}
```

同文件相关支撑类型：

```ts
export interface ConversationTimelineSnapshot {
  readonly turnOrder: readonly number[]
  readonly turns: ReadonlyMap<number, TurnLocation>
}

export interface ConversationViewBuilder<Node extends ConversationViewNode = ConversationViewNode, Snapshot = unknown> {
  readonly empty: Snapshot
  replace(input: {
    readonly nodes: readonly Node[]
    readonly timeline: ConversationTimelineSnapshot
  }): Snapshot
  apply(input: {
    readonly upserts: readonly Node[]
    readonly timeline: ConversationTimelineSnapshot
  }): Snapshot
}

export interface ConversationViewDefinition<Node extends ConversationViewNode = ConversationViewNode, Snapshot = unknown> {
  readonly target: string
  create(): ConversationViewBuilder<Node, Snapshot>
}

export function conversationContextKey(kind: string, id: string): string
```

---

## 未找到 / 备注

- 任务列出的 17 个 seam 全部找到并在上方列出，没有缺失。
- `SubprocessTerminalHandle` 的最后一个方法（幂等 terminate + await quiescence）签名在 `subprocess/subprocess/src/types.ts` 第 235 行起，我截取时行尾被截断，方法名可读源码确认（是 close/terminate 类终结方法）。
- `SettingsScope<T>` 不是命名 interface，是 `SettingsProvider.register()` 的内联返回对象字面量（已在上方注明其形状）。
- `ApprovalOutcome` 定义在 `interaction/user-approval/src/types.ts`，值域为 `'allowed-once' | 'rejected' | 'cancelled' | 'unavailable'`。
- cordis 的 `Context` 同时有 `interface Context`（服务属性声明合并目标）和 `class Context`（运行时构造）两个同名声明，这是源码原貌。
- 所有事件签名上方的 `@mode` 标注（waterfall/emit/serial/parallel/bail）来自源码 JSDoc，我保留在注释中以便判断该用 `ctx.on`/`ctx.waterfall`/`ctx.serial` 哪种方式接入。