先把当前现状摸清楚了，把我看到的事实和接下来的 plan 一起给你。

  当前真实状态盘点

  已经落地的部分
  - 重命名层：CommandRouter / CommandRouterHandle / SessionDirectory /
  SessionBootstrapper / AgentSessionCoordinator / ClientEventPublisher 都已就位。
  - SessionMessenger trait 进了
  crates/astrcode-core/src/tool.rs，SessionServices.with_session_messenger
  也接好了。
  - BoundSessionMessenger 在 crates/astrcode-server/src/handler/supervisor.rs
  里有了初版。
  - ActorEventBus 已经把 turn 事件回投到 actor，AgentSessionCoordinator 也开始走
  supervisor/handle 路径。
  - mailbox: VecDeque<String> 已写入 CommandRouter。

  没落地、且当前编译失败的部分（cargo check -p astrcode-server 5 个错误）
  - handler/supervisor.rs 用了 SessionActorHandle，但 actor.rs 没这个类型 —— 只有
  pub type SessionActor = CommandRouter 这个临时别名。
  - 4 处 crate::session_supervisor::... 的旧路径还没有改到
  crate::handler::supervisor::... 或 crate::handler::{SessionSupervisor,
  BoundSessionMessenger}：agent_session_coordinator.rs:35,71, handler/actor.rs:228,
  handler/turn.rs:437。
  - CommandRouter 还混着两个角色：route_session_commands: bool + active_turns:
  HashMap<SessionId, ActiveTurn> + mailbox，actor 和 router 没真正分家。
  - 子会话编排表面切到了 actor，但 prepare_child_session 仍直接调用
  parent_handle.emit_session_event(...) /
  child_handle.emit_session_event(...)，没经过 actor 内部的 turn 路径；并且 actor
  外部 parent.append_event 还存在。
  - 没有跨 session 消息的实际语义闭环：enqueue_message 在
  route_session_commands=true 的 router actor 里收到时 active_session_id 是
  None，会被丢弃。

  Plan：架构落地 + 目录重整一起做

  目标目录结构（最终态）

  crates/astrcode-server/src/
  ├── lib.rs
  ├── main.rs                    # 入口
  ├── http_main.rs
  ├── bootstrap.rs               # 仅做 ServerRuntime 装配
  ├── config_manager.rs
  ├── transport/                 # stdio JSON-RPC 适配
  ├── http.rs                    # 待考虑后续也拆 dir，但本 PR 不动
  ├── acp/                       # ACP 适配
  │
  ├── router/                    # 协议路由层（替代现在的 handler/）
  │   ├── mod.rs                 # CommandRouter：纯协议适配 + active_session_id
  │   ├── handle.rs              # CommandRouterHandle
  │   └── tests.rs
  │
  ├── session/                   # session 内部状态拥有者
  │   ├── mod.rs                 # 重新导出
  │   ├── directory.rs           # 移自 session_directory.rs
  │   ├── bootstrapper.rs        # 移自 session_bootstrapper.rs
  │   ├── supervisor.rs          # SessionSupervisor + 同树校验
  │   ├── handle.rs              # SessionHandle（薄壳，发 SessionCommand）
  │   ├── actor.rs               # SessionActor 主循环
  │   ├── command.rs             # SessionCommand 枚举
  │   ├── mailbox.rs             # 运行时 mailbox（FIFO + idle 续跑）
  │   ├── turn.rs                # TurnWorker（仅 run TurnRunner，事件回投）
  │   ├── slash.rs               # session 内 slash 处理
  │   ├── compact.rs             # session 内 compact 处理
  │   ├── snapshot.rs
  │   └── messenger.rs           # BoundSessionMessenger
  │
  ├── coordinator/
  │   └── agent.rs               # AgentSessionCoordinator（仅父子编排）
  │
  └── events/
      └── publisher.rs           # ClientEventPublisher

  要点：原 handler/ 拆成 router/ + session/；server 层只剩 router，session
  层归并所有 per-session 行为；coordinator 单独一层。

  实施顺序（一个 PR 内分步推进）

  1. 修当前编译错误，但不改语义
    - handler/supervisor.rs 的 SessionActorHandle 改成用现有
  CommandRouterHandle（先做别名 pub type SessionHandle = CommandRouterHandle
  留作演进用）。
    - 4 处 crate::session_supervisor::... 改到 crate::handler::{SessionSupervisor,
  BoundSessionMessenger}。
    - 验证：cargo check -p astrcode-server 通过。
  2. 提取真正的 SessionActor 类型
    - 新增 SessionActor 独立 struct，SessionCommand 独立 enum。
    - CommandRouter 不再持有 active_turns / mailbox / route_session_commands，只保留
   active_session_id 和到 supervisor 的引用；submit/abort/compact/slash 全部转发。
    - SessionActor 拥有 active_turn: Option<ActiveTurn> / mailbox: VecDeque<...> /
  直接持有 Arc<ServerRuntime> + Arc<ClientEventPublisher> + SessionId。
    - 验证：cargo test -p astrcode-server。
  3. 目录重整（机械搬迁）
    - handler/ → router/（mod.rs / handle.rs / tests.rs）。
    - session_directory.rs → session/directory.rs，session_bootstrapper.rs →
  session/bootstrapper.rs。
    - handler/supervisor.rs → session/supervisor.rs，再切出 session/messenger.rs、se
  ssion/mailbox.rs、session/handle.rs、session/command.rs、session/actor.rs、session
  /turn.rs、session/slash.rs、session/compact.rs、session/snapshot.rs。
    - agent_session_coordinator.rs → coordinator/agent.rs。
    - client_event_publisher.rs → events/publisher.rs。
    - lib.rs 重新导出，对外稳定 pub use 不破。
    - 验证：cargo build + cargo test -p astrcode-server。
  4. TurnWorker 收口 durable write
    - turn 后台任务唯一通道：把事件作为 SessionCommand::AppendEvent 投回
  actor；SessionActor::handle_append_event 才调 event_store.append_event +
  publisher.publish。
    - agent_turn_started/completed/failed payload 全走 actor。
    - AgentSessionCoordinator 不再调用 parent_handle.emit_session_event，改为
  SessionCommand::AppendChildSpawned/Completed/Failed，由对应 actor 写。
    - 加一个 server 层的 assert!(actor_only) 调试 helper（只在 cfg(test)
  下检查），代码搜索阶段把 actor 外部的 append_event 全清掉。
    - 验证：跑 compact / abort / child spawn 路径的现有集成测试。
  5. 统一主会话 / 子会话 / mailbox
    - prepare_child_session 拆成纯 prepare（不写事件），事件全走 actor。
    - BoundSessionMessenger.send 走 SessionSupervisor::deliver（同树校验 + 投递
  SessionCommand::EnqueueMessage）。
    - SessionActor 在 turn 结束后，从 mailbox 取下一条按普通 UserMessage 起
  turn；空闲时直接起。
    - 不持久化未消费消息；保持运行时 only。
  6. PROJECT_ARCHITECTURE.md 同步
    - 更新拓扑图：把 handler/ 替换成 router/ + session/ + coordinator/ + events/。
    - 写明 actor 唯一 durable writer 的不变量。
    - “Session messaging” 一节单列。

  验证策略

  按 CLAUDE.md 总则，最小相关检查：
  cargo fmt --check
  cargo clippy -p astrcode-server --all-targets -- -D warnings
  cargo test -p astrcode-server
  cargo test -p astrcode-session
  最后再补：
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test --all-features

  测试覆盖 plan 里列的：路由/会话行为、单 session
  串行性、父子统一、事件所有权、mailbox。handler/tests.rs 的 1455 行会跟着 router →
  session 切分相应迁移；新增测试只在“真实行为变化”处加（同树校验、mailbox
  续跑、actor 外部写入禁止）。

  ---
  关于“能否一直进行直到完成”

  直说：能完成，但需要你接受两个现实条件。

  1. 这是一个跨 6-7k 行 server 代码 + 1455 行测试的较大改动。中途 cargo test
  不可避免会反复红绿，我会一段段修。我不会半路停下，但我会按上面 6
  步分批推进，每一步本地通过编译/测试再走下一步。
  2. http.rs 1780 行和 handler/tests.rs 1455 行里有些断言锚定在旧名称 /
  旧路径上，这部分纯机械改动，量大但不复杂；如果你看到中途某一步测试改动量爆炸，我会
  停下来确认是否要把测试拆 PR 处理，而不是擅自瘦身。

  如果你确认就这样，我下一步会按第 1 步先把当前的 5
  个编译错误修掉，让你能在每个阶段稳定地观察进展，而不是在空中飞一整个大 PR。

  下一步建议
  1. 我先做第 1 步（修编译错误，不改语义），确认基线绿了再继续。
  2. 决定是否把目录搬迁单独做成第 3 步那样的纯 mv（对 review 友好）还是和第 4-5
  步一起合并。
  3. 如果你希望 http.rs 也拆成 dir，需要单独立一个跟进项，本 plan 不含。

  剩余风险
  1. SessionActor = CommandRouter 这个临时别名拆掉时，会触发大面积 impl
  CommandRouter 重新归属（slash/compact/turn 里的 impl CommandRouter 都得分流到
  SessionActor 或 CommandRouter），改动面比看上去大。
  2. handler/tests.rs 一些测试目前直接 spawn
  CommandRouter（同时跑路由+session），拆开后这些测试要么改成 spawn
  SessionActor，要么走 supervisor，可能影响断言。
  3. 异步 child agent 的 background_task 取消语义在 actor 化后需要重新审视：取消父
  turn 时是否要级联取消 mailbox 里属于 child 的待跑消息，plan 里没明确，建议在第 5
  步前先对齐。
  4. BoundSessionMessenger.send 现在是 fire-and-forget，错误只
  tracing::warn，未来要不要给工具一个回执 channel 也需要后续决定。

❯ /model sonnet                                                                     
  ⎿  Set model to claude-sonnet-4-6[1M] (claude-sonnet-4-6[1m])

❯ /model opus                                                                       
  ⎿  Set model to claude-opus-4-7-thinking[1M] (claude-opus-4-7-thinking[1m])

❯ <ide_opened_file>The user opened the file /home/whatevertogo/githubown/astrcode/a 
  strcodey/crates/astrcode-server/src/handler/mod.rs in the IDE. This may or may    
  not be related to the current task.</ide_opened_file>                             

❯ 是否有些职责应该放到astrcode-session里面？是否astrcode-session你也顺便整理一下？  

  Read 5 files, listed 1 directory (ctrl+o to expand)

● Failed to authenticate. API Error: 403 预扣费额度失败, 用户剩余额度: 🔪15.668556,
  需要预扣费额度: 🔪16.365100 (request id: 20260517075934300677928268d9d6vkX3cHdS)

❯ 是否有些职责应该放到astrcode-session里面？是否astrcode-session你也顺便整理一下？  
                                                                                   
  Searched for 2 patterns, read 6 files (ctrl+o to expand)                          
                                                                                  
● 把 astrcode-session 也读了一遍。说说我看到的边界错位，再给一个分两段的可选方案。  
                                                                                    
  astrcode-session 现状盘点                                                         
                                                                                    
  astrcode-session/src/  (3955 行，扁平 16 个 .rs)                                  
  ├── session.rs               (215) Session 句柄：写 durable event 的入口          
  ├── session_runtime.rs        (47) SessionRuntimeRegistry / FileObservationStore  
  占位                                                                              
  ├── session_services.rs       (69) SessionServices DI 容器                        
  ├── turn_runner.rs           (541) TurnRunner / drive_agent / run_turn            
  ├── turn_context.rs          (101) EventBus / SharedTurnContext / TurnError       
  ├── llm_stream.rs            (205)                                                
  ├── tool_pipeline.rs         (639)                                                
  ├── tool_exec.rs             (504) 后台化 / FileObservationStore 实现             
  ├── tool_types.rs             (85)                                                
  ├── background.rs            (307) BackgroundTaskManager                          
  ├── compact.rs               (119) PreCompact/PostCompact hook 桥接               
  ├── post_compact.rs          (519) Post-compact context 恢复                      
  ├── payload.rs               (119) turn started/completed payload helpers         
  ├── mcp_visibility.rs        (107)                                                
  └── json_repair.rs           (348)                                                
                                                                                    
  当前职责错位的清单                                                                
                                                                                    
  按"session 的事被放在了 server"这条线索，我看到 6 处：                            
                                                                  
  #: 1                                                                              
  现在位置: handler/actor.rs SessionActor/CommandRouter 的 per-session 写入路径 
  真正归属: astrcode-session                                                        
  原因: actor 是"Session owns facts, SessionActor owns mutations"的具象，session
    crate 是它的家                                                              
  ────────────────────────────────────────                                          
  #: 2                                                                          
  现在位置: handler/turn.rs run_agent_turn_task + ActiveTurn + mailbox              
  真正归属: astrcode-session                                      
  原因: 驱动 TurnRunner、收 turn lifecycle event 这些是 session-runtime 行为；server
   
    里只剩"把回调投回 actor"的胶水                                                  
  ────────────────────────────────────────                        
  #: 3                                                                              
  现在位置: handler/compact.rs compact_session() 整个流水线
  真正归属: astrcode-session（新增 compact_runner.rs）                              
  原因: snapshot                                                  
    写入、compact_messages_with_fallback、enrich_post_compact_context、dispatch_post
  _compact、append_compact_boundary
     这些都在 session crate 里有半成品，主流程却在 server
  ────────────────────────────────────────
  #: 4                                                                              
  现在位置: handler/turn.rs repair_stale_pending_tool_calls
  真正归属: astrcode-session                                                        
  原因: 纯 session-runtime 恢复逻辑，server 不该知道 Phase::CallingTool 等 session
    内部状态
  ────────────────────────────────────────
  #: 5                                                                              
  现在位置: session_bootstrapper.rs 的 tool registry 缓存 + system prompt 组装
  真正归属: astrcode-session（部分）                                                
  原因: tool registry 是 session 级快照，prompt 组装也是；但 ConfigManager 留
    server，需要把它抽成一个小 trait（如 ModelDefaultsProvider）注入
  ────────────────────────────────────────
  #: 6                                                                              
  现在位置: agent_session_coordinator.rs 的 child turn 启动
  真正归属: server 留协调，session 提供 actor 入口                                  
  原因: 父子编排是 server 关心的，但启动子 turn 应该走 session 的 
    SessionHandle.submit_input 而不是 server 自己 spawn 一个 actor

  astrcode-session 自身可以顺手整理的                                               
   
  只是分目录 + 把现在挤在一起的几个文件按职责重组：                                 
                                                                  
  astrcode-session/src/                                                             
  ├── lib.rs                                                      
  ├── session.rs                 # 保留：Session handle + SessionError              
  ├── services.rs                # 重命名 session_services.rs → services.rs         
  ├── runtime.rs                 # 重命名 session_runtime.rs → runtime.rs           
  │                                                                                 
  ├── actor/                     # ★ 新增                                           
  │   ├── mod.rs                 # SessionActor 入口                                
  │   ├── handle.rs              # SessionHandle + SessionCommand                   
  │   ├── mailbox.rs             # FIFO mailbox + idle 续跑                         
  │   └── turn_state.rs          # ActiveTurn                                       
  │                                                                                 
  ├── turn/                      # ★ 新增（拆 turn_runner.rs / turn_context.rs /    
  llm_stream.rs）                                                                   
  │   ├── mod.rs                                                  
  │   ├── runner.rs              # TurnRunner                                       
  │   ├── driver.rs              # drive_agent / run_turn         
  │   ├── context.rs             # EventBus / SharedTurnContext / TurnError         
  │   ├── worker.rs              # ★ 新增：把 server 的 run_agent_turn_task 拉过来  
  │   ├── llm_stream.rs                                                             
  │   └── payloads.rs            # 重命名 payload.rs，只放 turn 相关                
  │                                                                                 
  ├── tool/                      # ★ 新增（拆 tool_pipeline / tool_exec / tool_types
   / background / mcp_visibility）                                                  
  │   ├── mod.rs                                                  
  │   ├── pipeline.rs                                                               
  │   ├── exec.rs                                                                   
  │   ├── types.rs                                                                  
  │   ├── background.rs                                                             
  │   └── mcp_visibility.rs                                                         
  │                                                                                 
  ├── compact/                   # ★ 新增                                           
  │   ├── mod.rs                                                                    
  │   ├── hooks.rs               # 原 compact.rs 内容                               
  │   ├── post_context.rs        # 重命名 post_compact.rs                           
  │   ├── payloads.rs            # 原 payload.rs 中 compact 相关                    
  │   └── runner.rs              # ★ 新增：把 server 的 compact_session 流水线拉过来
  │                                                                                 
  ├── repair.rs                  # ★ 新增：repair_stale_pending_tool_calls          
  ├── bootstrapper.rs            # ★ 从 server 移过来：per-session tool / prompt    
  准备                                                                              
  └── infra/                                                                        
      └── json_repair.rs                                                            
                                                                                    
  抽象的取舍
                                                                                    
  为了把上面 6 项搬过来，session crate 需要新增 2 个 trait 和 1 个 type             
  alias，避免反向依赖 server：
                                                                                    
  1. EventSink（已有 EventBus，可复用，只是改名以表达"session-runtime 已知 durable +
   broadcast"语义不变）
  2. ModelDefaultsProvider（trait）—— read_effective_model_id() /                   
  read_effective_timeout()，server 用 ConfigManager 实现                            
  3. SessionHandle：薄壳，mpsc::UnboundedSender<SessionCommand> 包装
                                                                                    
  不引入 的：                                                                       
  - 不把 SessionSupervisor 移到 session（它要做"按 session id 路由 actor"，是 server
   的协调职责）                                                                     
  - 不把 BoundSessionMessenger 实现移到 session（实现绑定到 supervisor，留 server）
  - 不把 AgentSessionCoordinator 移到 session（父子编排是 server 关心的事）         
                                                                                    
  依赖方向仍严格单向：astrcode-server → astrcode-session → astrcode-context /       
  astrcode-tools / astrcode-extensions / astrcode-core。                            
                                                                                    
  两种 PR 切分方案                                                                  
                                                                  
  方案 A（一次性大 PR）：上次 plan 的 6 步 + session crate 拆分 + 6                 
  项职责迁移一起做。                                              
  - 优点：架构边界一次到位，PROJECT_ARCHITECTURE.md 一次写定。                      
  - 缺点：跨 4 个 crate（server / session / 测试 / 文档）改动量爆炸；中途 cargo test
   会持续红一段时间；review 困难。                                                  
  - 估算：原本 6-7k 行 server + 1455 行测试，加上 ~4k 行 session + 跨 crate import  
  改动，至少 1.5x。                                                               
                                                                                    
  方案 B（分两次）——我推荐这个：                                  
  - PR-1（上次 plan）：在 server 内部完成 actor 化 + handler/ → router/ + session/ +
   coordinator/ + events/ 的目录重整。SessionActor 仍暂留 server。唯一从 session    
  crate 反向引用 server 的概念（durable event 写入）保持原状。                      
  - PR-2（这次新增）：把上面 6 项职责从 server 迁到 session，同时拆分 session crate 
  的目录。新增 actor/ turn/worker.rs compact/runner.rs repair.rs                   
  bootstrapper.rs，server 的 session/actor.rs 退化成 trait 实现 + 胶水。            
                                                                        
  为什么推荐 B                                                                      
                                                                                    
  1. PR-1 的目标是修边界混乱：让 actor 真正存在、durable write                      
  收口、主子统一。这一步即使 actor 主体留 server，对外行为已经稳定。                
  2. PR-2 是纯搬家 + 提取 trait：在 PR-1 落地、actor                                
  边界清晰后，搬家就是机械操作，编译循环短、风险低。                                
  3. CLAUDE.md 强调"不要新增 trait、DTO 除非必要"：PR-1 不需要新 trait，PR-2 才引入
  ModelDefaultsProvider，这种顺序更容易反思每个抽象是否值得。                       
  4. 方案 A 中途如果撞上某个跨 crate trait 设计需要返工，整盘要回滚。