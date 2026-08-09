# pi-coding 架构图（Mermaid）

> 来源：对 `crates/pi-coding`（约 128K 行 Rust）的结构分析。所有引用以 `<repo-root>/crates/...` 相对路径 + 行号给出，可执行验证：`grep -n "<symbol>" crates/pi-coding/src/<file>.rs`。

## 图 0 — 四层 crate 依赖

```mermaid
flowchart TB
    subgraph L0["pi-cli — crates/pi-cli"]
        TUI["TUI 面板 / REPL<br/>tui.rs · repl.rs"]
        RPC["JSON-RPC / ACP 模式<br/>rpc.rs · acp.rs"]
        CMD["子命令 · 会话编排<br/>interactive_commands.rs · session_run.rs"]
    end
    subgraph L1["pi-coding — crates/pi-coding（本图主体）"]
        APP["Application 状态机<br/>application.rs"]
        SESS["Session 回合执行<br/>session.rs"]
        SUB["工具 · 编排 · 工作流<br/>扩展 · 配置 · 持久化"]
    end
    subgraph L2["pi-agent — crates/pi-agent"]
        AGENT["Agent 循环<br/>agent.rs · loop_runtime.rs"]
    end
    subgraph L3["pi-ai — crates/pi-ai"]
        PROV["providers/<br/>anthropic · openai · responses · codex · gemini..."]
        CAT["模型目录 · 流式 · 重试 · 超时<br/>catalog.rs · stream.rs"]
    end
    TUI --> APP
    RPC --> APP
    CMD --> APP
    APP --> SESS
    SESS --> AGENT
    AGENT --> PROV
    PROV --> CAT
    classDef lay fill:#eef7ff,stroke:#1565c0
    class L0,L1,L2,L3 lay
```

依赖方向（AGENTS.md 约束）：`pi-cli → pi-coding → pi-agent → pi-ai`，禁止跨层直取内部实现。

## 图 1 — pi-coding 内部模块全景

```mermaid
flowchart LR
    subgraph CORE["回合核心"]
        APP1["Application 状态机<br/>application.rs:335<br/>+ application/runtime.rs"]
        SESS1["Session<br/>session.rs:717<br/>turn loop · retry · compaction"]
        STORE["session_store.rs<br/>SessionRecorder:607 · start_session:1177<br/>resume:1551 · fork:1473"]
    end
    subgraph TOOLS["工具子系统 tools.rs + tools/*"]
        CAT["工具目录<br/>tools.rs:537 create_all_tools"]
        BASH["bash/brush · process/<br/>ProcessManager:23"]
        EDIT["edit · write · ast_edit<br/>editdiff · editmatch"]
        LSP1["lsp · lsp_client"]
        BROWSER["browser · web_search"]
        EVAL["eval · notebook · debug"]
        IMG["image · image_gen · imageresize"]
        MEMTOOL["memory · recall · retain · reflect"]
        MCPTOOL["mcp_tool（McpRegistry）<br/>mcp.rs:554"]
    end
    subgraph AGENT2["子代理编排 orchestration/"]
        ORCH["OrchestrationRuntime<br/>orchestration/runtime.rs:730"]
        CHILD["ChildSession<br/>orchestration/runtime.rs:57"]
        JOBS["jobs · persistence<br/>工具绑定"]
    end
    subgraph WF["工作流 workflow/ + workflow_worktree/"]
        WFM["WorkflowManager<br/>workflow/manager.rs:196"]
        WFS["WorkflowSupervisor<br/>workflow/supervisor.rs:283"]
        WFT["worktree overlay<br/>workflow_worktree/mod.rs:280"]
    end
    subgraph EXT["扩展与集成"]
        EXT1["extensions.rs<br/>ExtensionSpec:564 · 进程/QuickJS"]
        QJS["quickjs_host.rs<br/>QuickJsExtensionHost:630"]
        PLUGIN["plugin.rs 市场"]
        PKG["packages.rs 包资源"]
        HOOKS["hooks.rs HostHooks:71"]
    end
    subgraph CFG["配置 · 安全 · 隔离"]
        SET["settings.rs · settings_catalog.rs<br/>RuntimeSettingsSnapshot:517"]
        AUTH["auth.rs AuthManager:921"]
        TRUST["trust.rs TrustStore:172<br/>resolve_project_trust:362"]
        SANDBOX["sandbox.rs SandboxConfig:48"]
        ENC["encrypt.rs · oauth.rs"]
    end
    subgraph CTX["上下文与选择"]
        SEL["selector.rs SelectionPlan:321"]
        RES["resources.rs · resource_manager.rs<br/>ResourceManager:317"]
        MEM["memory.rs MemoryConfig:588"]
        SYS["system_prompt.rs · prompt_templates.rs"]
    end
    subgraph DUR["持久化 · 生命周期"]
        COMP["compaction.rs 压缩/回退"]
        SCAT["session_catalog/ 会话树"]
        LOOP1["loop_scheduler.rs LoopTask:97"]
        GOAL["goal.rs · handoff.rs · todo.rs"]
    end
    APP1 --> SESS1
    SESS1 --> STORE
    SESS1 --> TOOLS
    SESS1 --> CTX
    SESS1 --> AGENT2
    AGENT2 --> WF
    APP1 --> EXT
    EXT --> QJS
    EXT --> PLUGIN
    EXT --> PKG
    APP1 --> CFG
    CFG --> AUTH
    CFG --> TRUST
    CFG --> SANDBOX
    SESS1 --> DUR
    DUR --> COMP
    DUR --> SCAT
```

## 图 2 — 单回合运行时流水线（Sequence）

```mermaid
sequenceDiagram
    autonumber
    actor U as 用户
    participant AP as "Application<br/>application.rs:1402"
    participant SE as "Session<br/>session.rs:3685 run()"
    participant AG as "pi-agent Agent<br/>agent.rs / loop_runtime.rs"
    participant PR as "pi-ai provider<br/>stream"
    participant TL as "Tool 目录<br/>tools.rs"

    U->>AP: prompt(text)
    AP->>SE: run(prompt)
    SE->>SE: run_messages(messages)
    SE->>SE: inject_selection_messages()<br/>selector 选择 + memory 注入
    SE->>SE: begin_run() → ClaimedRun
    SE->>SE: execute_with_retries()<br/>session.rs:3853
    SE->>AG: agent.prompt_messages()
    AG->>AG: run_agent_loop → run_loop
    loop 每个模型回合
        AG->>PR: 流式请求（含工具定义）
        PR-->>AG: assistant 消息 / tool_call
        alt 有工具调用
            AG->>TL: 执行 AgentTool.execute()
            TL-->>AG: ToolResult
            AG->>AG: 工具结果回合继续
        else 无工具调用
            AG-->>SE: 回合结束（stop_reason）
        end
    end
    SE->>SE: finish_run() → 记录/事件
    SE-->>AP: RunResult
    AP-->>U: 渲染结果
```

## 图 3 — 回合失败恢复决策（重试 / 回退 / 压缩）

```mermaid
flowchart TD
    A["execute_with_retries<br/>session.rs:3853"] --> B{"最后消息是<br/>Assistant 成功?"}
    B -->|是| Z["返回成功<br/>finish_retry_success"]
    B -->|否| C{"stop_reason == Error?"}
    C -->|否| Z
    C -->|是| D{"context overflow?"}
    D -->|是| E{"已尝试过<br/>overflow 恢复?"}
    E -->|是| F["报错终止<br/>Context overflow persisted"]
    E -->|否| G["自动压缩<br/>perform_compaction(Overflow)"] --> H["继续 agent.continue_run()"] --> B
    D -->|否| I{"retryable 错误<br/>且未超限?"}
    I -->|是| J["重试（RetrySettings）<br/>session.rs RetrySettings:467"]
    J --> B
    I -->|否| K{"有回退链候选?<br/>retry_fallback.rs find_candidates:409"}
    K -->|是| L["切换到回退模型<br/>format_retry_fallback_selector:112"]
    L --> M["continue_run"] --> B
    K -->|否| N["DoomLoopTracker 触发<br/>session.rs:228 → 终止"]
    N --> F
```

## 图 4 — pi-agent 循环内部

```mermaid
flowchart TB
    E["run_agent_loop<br/>loop_runtime.rs:45"] --> F["run_loop<br/>loop_runtime.rs:103"]
    F --> G["取 steering / follow-up 消息<br/>PendingQueue"]
    G --> H["组装 AgentContext<br/>types.rs:546"]
    H --> I["调用 stream_fn（pi-ai 流式）"]
    I --> J{"解析回复"}
    J -->|tool_call| K["before_tool_call 钩子"]
    K --> L["执行 AgentTool（AgentTool::execute<br/>types.rs:265）"]
    L --> M["after_tool_call 钩子"]
    M --> N["写入消息历史"] --> I
    J -->|纯文本| O["AgentSettled 事件"]
    J -->|Aborted/Error| O
    O --> P["AgentEvent 广播<br/>types.rs:645"]
    P --> Q["Session 订阅 → ApplicationEvent → TUI"]
```

## 图 5 — 工具子系统

```mermaid
flowchart LR
    S["Session.get_active_tools<br/>session.rs:1942"] --> CAT["create_all_tools<br/>tools.rs:537"]
    CAT --> T1["read / grep / find / glob / ls"]
    CAT --> T2["bash（brush 引擎 + ProcessManager）<br/>tools/bash · process/"]
    CAT --> T3["edit / write / ast_edit<br/>tools/ast_grep · editdiff · editmatch"]
    CAT --> T4["lsp / browser / web_search"]
    CAT --> T5["eval / notebook / debug"]
    CAT --> T6["image / image_gen / imageresize"]
    CAT --> T7["memory / recall / retain / reflect<br/>memory.rs"]
    CAT --> T8["mcp_tool → McpRegistry<br/>mcp.rs:656"]
    CAT --> T9["ask / todo / goal"]
    subgraph INFRA["执行基础设施"]
        I1["tool_presentation.rs<br/>结果渲染与卡片"]
        I2["truncate.rs · redact.rs<br/>截断与脱敏"]
        I3["tools/mutation_queue.rs<br/>写操作队列"]
        I4["tools/framing.rs<br/>流式帧"]
    end
    T2 --> INFRA
    T3 --> INFRA
    CAT --> INFRA
```

## 图 6 — 子代理编排（Orchestration）

```mermaid
flowchart TB
    APP2["Application / Session"] --> OR["OrchestrationRuntime<br/>orchestration/runtime.rs:730"]
    OR --> CS["ChildSession<br/>runtime.rs:57<br/>（独立 Session 快照）"]
    OR --> CHILDREQ["ChildSessionRequest<br/>runtime.rs:532<br/>（任务 + 预算 + 工具）"]
    OR --> MAIL["MailboxMessage<br/>runtime.rs:618<br/>子代理消息信箱"]
    OR --> DUR2["PreparedDurableBinding<br/>runtime.rs:920<br/>持久化绑定（fail-closed）"]
    OR --> TOOLS2["orchestration/tools.rs<br/>子代理控制工具"]
    CS --> SNAP["AgentSnapshot · AgentStatus<br/>runtime.rs:573"]
    CS --> RESULT["TaskResult<br/>runtime.rs:709"]
    TOOLS2 --> OR
    DUR2 --> STORE2["session_store 持久子会话<br/>start_durable_child_session_in<br/>session_store.rs:1560"]
```

## 图 7 — 工作流（YAML DAG + worktree）

```mermaid
flowchart TB
    WM["WorkflowManager<br/>workflow/manager.rs:196"] --> PARSE["解析 YAML DAG<br/>workflow/detail.rs · store.rs"]
    PARSE --> TASKS["WorkflowTask DAG<br/>WorkflowStatus:54"]
    TASKS --> SUP["WorkflowSupervisor<br/>workflow/supervisor.rs:283"]
    SUP --> AGENTS3["按任务分配子代理<br/>（orchestration ChildSession）"]
    SUP --> TOB["WorkflowSupervisorTodoObservation<br/>supervisor.rs:73<br/>观察 todo 完成度"]
    SUP --> EV3["WorkflowEvent 广播"]
    subgraph WT["worktree 隔离"]
        WTM["WorkflowWorktreeManager<br/>workflow_worktree/mod.rs:280"]
        OVER["overlay.rs 工作树叠加"]
        GIT["git.rs 集成（IntegrateStrategy:209）"]
    end
    WM --> WTM
    WTM --> OVER
    OVER --> GIT
    GIT -->|IntegrateOutcome| WM
```

## 图 8 — 扩展 / 钩子 / 信任 / 沙箱 / MCP

```mermaid
flowchart TB
    APPH["ApplicationExtensionHost<br/>application.rs:3067"] --> EXTS["ExtensionSpec<br/>extensions.rs:564"]
    EXTS --> RUNTIME{"runtime 类型"}
    RUNTIME -->|process| PE["ProcessExtensionManifest<br/>extensions.rs:227<br/>子进程扩展"]
    RUNTIME -->|quickjs| QE["QuickJsExtensionHost<br/>quickjs_host.rs:630"]
    PE --> CAP["ExtensionCapabilityManifest<br/>extensions.rs:122"]
    QE --> CAP
    CAP --> PERM["ExtensionPermissionSet<br/>extensions.rs:154"]
    PERM --> TRUSTDEC{"pre_trust_decision<br/>（宿主钩子先行）"}
    TRUSTDEC -->|允许| LOAD["加载并执行扩展<br/>工具 · 事件钩子 · 渲染器"]
    TRUSTDEC -->|拒绝/询问| ASK["Ask 升级为 Trusted<br/>（trust 只能升不能降）"]
    TRUSTDEC --> SANDBOX2["sandbox.rs resolve:499<br/>沙箱配置（cap-std）"]
    PERM --> HOOKS2["HostHooks 事件<br/>hooks.rs:71<br/>before_tool_call 等"]
    PERM --> MCP2["McpRegistry 服务器<br/>mcp.rs:554<br/>stdio 子进程（可禁用）"]
    LOAD --> TOOLP["扩展工具注入 Session 工具目录"]
```

## 图 9 — 会话持久化与生命周期

```mermaid
flowchart TB
    START["start_session<br/>session_store.rs:1177"] --> REC["SessionRecorder<br/>session_store.rs:607<br/>（消息 + 工具调用 + 事件）"]
    REC --> FILES["会话文件<br/><repo-digest>/<session-id>/"]
    FILES --> RESUME["resume_session<br/>session_store.rs:1551"]
    FILES --> BRANCH["create_branched_session<br/>session_store.rs:1237"]
    FILES --> FORK["fork_session_in<br/>session_store.rs:1473"]
    REC --> COMP["compaction.rs<br/>（阈值/手动/溢出压缩）"]
    COMP --> REWIND["RewindTarget · RewindOutcome<br/>session.rs:589"]
    COMP --> SNAP["compact_snap 快照压缩<br/>（先落盘+fsync 再引用）"]
    FILES --> SCAT["session_catalog/<br/>会话树 · lineage"]
    REC --> EVENTS["SessionEvent 广播<br/>session.rs:503 → ApplicationEvent:209"]
```

## 图 10 — 上下文装配管线（选择器 + 资源 + 系统提示词）

```mermaid
flowchart LR
    REQ["用户请求"] --> SEL["selector.rs<br/>SelectionPlan:321<br/>（autoMode / 分类 / 技能选择）"]
    SEL --> AUTO["PromptMode · AutoMode<br/>selector.rs:82"]
    SEL --> SKILL["加载 skill:// 技能正文<br/>session.rs inject_selection_messages"]
    REQ --> RES["resources.rs / resource_manager.rs<br/>ResourceSnapshot:259<br/>可信项目资源发现"]
    RES --> RELOAD["reload_resources<br/>session.rs:1744"]
    REQ --> MEM["memory.rs<br/>（hindsight 记忆注入）"]
    MEM --> INJ["inject_hindsight_memory"]
    REQ --> SYS["system_prompt.rs<br/>当前系统提示词 session.rs:1991"]
    SYS --> TEMPLATES["prompt_templates.rs"]
    AUTO --> SESS3["Session 回合开始"]
    SKILL --> SESS3
    INJ --> SESS3
    SYS --> SESS3
```

## 关键事实索引（可执行验证）

| 符号 | 位置 |
|---|---|
| `Session::new` | `crates/pi-coding/src/session.rs:723` |
| `Session::run` | `crates/pi-coding/src/session.rs:3685` |
| `execute_with_retries` | `crates/pi-coding/src/session.rs:3853` |
| `Application::new` / `Application::prompt` | `crates/pi-coding/src/application.rs:486` / `:1402` |
| `Agent`（pi-agent） | `crates/pi-agent/src/agent.rs:229` |
| `run_agent_loop` / `run_loop` | `crates/pi-agent/src/loop_runtime.rs:45` / `:103` |
| `create_all_tools` | `crates/pi-coding/src/tools.rs:537` |
| `OrchestrationRuntime` / `ChildSession` | `crates/pi-coding/src/orchestration/runtime.rs:730` / `:57` |
| `WorkflowManager` / `WorkflowSupervisor` | `crates/pi-coding/src/workflow/manager.rs:196` / `supervisor.rs:283` |
| `WorkflowWorktreeManager` | `crates/pi-coding/src/workflow_worktree/mod.rs:280` |
| `ExtensionSpec` / `QuickJsExtensionHost` | `crates/pi-coding/src/extensions.rs:564` / `quickjs_host.rs:630` |
| `McpRegistry` / `mcp_tool` | `crates/pi-coding/src/mcp.rs:554` / `:656` |
| `SessionRecorder` / `start_session` | `crates/pi-coding/src/session_store.rs:607` / `:1177` |
| `AuthManager` / `TrustStore` / `SandboxConfig` | `auth.rs:921` / `trust.rs:172` / `sandbox.rs:48` |
| `SelectionPlan` / `ResourceManager` / `MemoryConfig` | `selector.rs:321` / `resource_manager.rs:317` / `memory.rs:588` |
| `ProcessManager` | `crates/pi-coding/src/process/manager.rs:23` |
| `RuntimeSettingsSnapshot` | `crates/pi-coding/src/settings.rs:517` |

## 覆盖说明

- 全量阅读：`lib.rs` 模块导出、`session.rs`（回合/重试/压缩主路径）、`application.rs`（状态机与事件）、`tools.rs` 目录、`pi-agent` 循环、`session_store.rs` 持久化、`orchestration/runtime.rs`、`workflow/*`、`extensions.rs`、`auth/trust/sandbox` 核心类型。
- 摘要阅读（仅结构）：`settings.rs`、`mcp.rs`、`selector.rs`、`resource_manager.rs`、`compaction.rs`、`plugin.rs`、`packages.rs`、`markdown/*`。
- 未逐行展开：各 `tools/*` 内部实现、`tests.rs`、`session_catalog/tests.rs`。
