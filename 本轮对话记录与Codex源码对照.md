# 本轮对话记录与 Codex 源码对照（面向 Rust Agent 小白）

> 目的：把“概念”落到代码上，重点解释 `run_turn` / `handle_output_item_done` 在 Codex CLI 里的真实职责，并给出一份“教学版最小 Agent”与仓库实现的逐项对照。

---

## 1. 先把两个名字翻译成人话

### 1.1 `run_turn`：跑完“向模型发起一次请求”的整轮

- **turn** 在 Codex 的语境里，基本等价于：**把当前对话历史（含工具输出）+ 工具清单发给模型一次，并把模型的流式响应消费到 `response.completed`**。
- 源码入口：`codex-rs/core/src/codex.rs:2367`

### 1.2 `handle_output_item_done`：处理“模型刚刚完成的一个输出项”

- 模型是流式输出的：会不断产生“输出项”（普通消息、推理块、web_search、工具调用……），每个输出项最后会有一个“done”时刻。
- `handle_output_item_done` 的职责是：**拿到一个“已完成的输出项”，判断它是不是工具调用；如果是，就把工具调用入历史并排队执行工具；如果不是，就把它当普通内容入历史并更新 UI 所需的 TurnItem。**
- 源码入口：`codex-rs/core/src/stream_events_utils.rs:43`

---

## 2. 教学版最小 Agent 与 Codex CLI 源码逐项对照表

下面对照基于你上一条需求里那份“教学版最小 Agent（Rust 单文件）”的结构（`Session/history`、`run_task/run_turn/handle_output_item_done`、`ToolRouter/ToolRegistry` 等）。

### 2.1 “任务循环（Task）”对照：为什么能迭代到收敛

| 教学版概念/函数 | 教学版含义 | Codex 实现 | 源码位置 | 关键差异 |
|---|---|---|---|---|
| `run_task(...)` | 外层循环：只要本轮产生过工具调用就继续下一轮 | `run_task(...)` | `codex-rs/core/src/codex.rs:2207` | Codex 会构造 turn 输入为“历史快照 + pending_input”，并处理 token 逼近上限时的 compact（见 `needs_follow_up` 分支） |
| `needs_follow_up` | “还要不要再来一轮”开关 | `TurnRunResult.needs_follow_up` | `codex-rs/core/src/codex.rs:2475` 及 `codex-rs/core/src/codex.rs:2313` | Codex 的 `needs_follow_up` 来自 `handle_output_item_done`：一旦出现 tool call 就置 true |

> 你在教学版里用 `loop { run_turn(); if !needs_follow_up break }`，对应的就是 Codex 的 `run_task` 主循环（完全同一个“闭环形状”）。

### 2.2 “Turn（一次模型请求）”对照：为什么叫 `run_turn`

| 教学版概念/函数 | 教学版含义 | Codex 实现 | 源码位置 | 关键差异 |
|---|---|---|---|---|
| `run_turn(...)` | 一次“模型决策”的执行：拿到模型输出项并逐个处理 | `run_turn(...)` → `try_run_turn(...)` | `codex-rs/core/src/codex.rs:2367`、`codex-rs/core/src/codex.rs:2506` | Codex 是流式事件驱动（`ResponseEvent`），还会合并 MCP 工具并声明 tools schema |
| `model.produce_items()` | 教学版伪模型一次产出一组输出项 | `ModelClient::stream(prompt)`（流式） | `codex-rs/core/src/codex.rs:2550` 附近 | 真实模型输出是事件流：`OutputItemAdded/Done` + delta + `Completed` |

### 2.3 “输出项 done”对照：`handle_output_item_done` 到底处理什么

| 教学版概念/函数 | 教学版含义 | Codex 实现 | 源码位置 | 关键差异 |
|---|---|---|---|---|
| `handle_output_item_done(...)` | 处理一个输出项：若是工具调用则执行工具并写回 ToolOutput | `handle_output_item_done(...)` | `codex-rs/core/src/stream_events_utils.rs:43` | Codex 会把工具执行包装成 future 放入 `in_flight`，并在 turn 结束前 `drain_in_flight` |
| `ToolRouter::build_tool_call(...)` | 从输出项中识别工具调用 | `ToolRouter::build_tool_call(...)` | `codex-rs/core/src/tools/router.rs:59` | Codex 支持多种 tool call 形态：FunctionCall / CustomToolCall / LocalShellCall，并能识别 MCP 工具名 |
| “不是工具调用就当普通消息” | 记录 assistant message，任务可能结束 | `handle_non_tool_response_item` + `parse_turn_item` | `codex-rs/core/src/stream_events_utils.rs:101`、`codex-rs/core/src/event_mapping.rs:71` | Codex 还会发 `TurnItemStarted/Completed` 等事件给 UI，并把内容写入 rollout/history |

补充：Codex 在流里还处理 `ResponseEvent::OutputItemAdded`（开始出现一个 item）与各种 delta（文本/推理增量），见 `codex-rs/core/src/codex.rs:2580` 附近；而“done”时刻才进入 `handle_output_item_done` 做“工具判定/入历史/排队执行”。

### 2.4 工具系统对照：spec → router → registry → handler → orchestrator

| 教学版模块 | 教学版含义 | Codex 实现 | 源码位置 | 说明 |
|---|---|---|---|---|
| `ToolRegistry` | `tool_name -> handler` 分发表 | `ToolRegistry::dispatch(...)` | `codex-rs/core/src/tools/registry.rs:61` | 统一分发入口，找不到工具会返回“RespondToModel”错误（等价于让模型收到失败信息） |
| `ToolRouter` | 从模型输出里解析 call 并路由 | `ToolRouter` | `codex-rs/core/src/tools/router.rs:18` | `build_tool_call` + `dispatch_tool_call` 两段式 |
| `ToolHandler` trait | 每个工具的具体实现 | `ToolHandler` trait | `codex-rs/core/src/tools/registry.rs:20` | 工具可声明 `is_mutating`，供门禁/并发控制使用 |
| `ShellTool` | 执行命令并返回输出文本 | `ShellHandler` / `ShellCommandHandler` | `codex-rs/core/src/tools/handlers/shell.rs` | 会走统一编排器，发 ExecCommandBegin/End 事件，并支持 apply_patch 拦截 |
| `ApplyPatchTool` | 修改文件内容 | `ApplyPatchHandler` | `codex-rs/core/src/tools/handlers/apply_patch.rs` | 会先校验补丁 grammar/正确性，然后可能委托到 exec 以适配平台差异 |
|（教学版无）`ToolOrchestrator` |（无） | `ToolOrchestrator::run(...)` | `codex-rs/core/src/tools/orchestrator.rs:34` | 统一处理：审批 → 选择 sandbox → 执行 →（策略允许时）无沙箱重试 |

### 2.5 “并发 + in_flight 排干”对照：为什么 Codex 不会丢工具输出

| 教学版做法 | Codex 做法 | 源码位置 | 说明 |
|---|---|---|---|
| 同步执行工具，立刻写入 ToolOutput | 把工具执行包装成 future 推入 `in_flight`，turn 末尾 `drain_in_flight` | `codex-rs/core/src/codex.rs:2542`、`codex-rs/core/src/codex.rs:2702` | 这是为了支持：模型继续流式输出时工具并行跑；同时保证 turn 结束前工具输出都入历史 |
| 无并发控制 | `ToolCallRuntime::handle_tool_call` 用读写锁区分“可并行工具/不可并行工具” | `codex-rs/core/src/tools/parallel.rs:49` | 工具 spec 可声明 `supports_parallel_tool_calls`，router 会据此决定并行或串行 |

### 2.6 “历史（history）”对照：为什么下一轮模型一定能看到工具输出

| 教学版概念 | Codex 对应 | 源码位置 | 说明 |
|---|---|---|---|
| `Session.history: Vec<ResponseItem>` | `record_conversation_items`（写入内存历史 + rollout + 事件） | `codex-rs/core/src/codex.rs:1168` | 这是“闭环”的关键：工具输出会变成下一轮 prompt 的一部分 |
| 直接把全部历史塞给模型 | `ContextManager` 负责历史管理/截断 | `codex-rs/core/src/codex.rs:1191`（重建）以及 `codex-rs/core/src/context_manager/...` | Codex 还会基于模型上下文窗口、截断策略做裁剪/compact |

### 2.7 “事件（可观测性）”对照：为什么它像一个可调试的运行时

| 行为 | 事件 | 源码位置 |
|---|---|---|
| 命令开始/结束 | `ExecCommandBegin` / `ExecCommandEnd` | `codex-rs/core/src/tools/events.rs:73`、`codex-rs/core/src/tools/events.rs:413` |
| 打补丁开始/结束 | `PatchApplyBegin` / `PatchApplyEnd` | `codex-rs/core/src/tools/events.rs:184`、`codex-rs/core/src/tools/events.rs:443` |
| 聚合 diff 输出 | `TurnDiff` | `codex-rs/core/src/codex.rs:2710`（turn 末尾） |
| MCP 工具调用开始/结束 | `McpToolCallBegin` / `McpToolCallEnd` | `codex-rs/core/src/mcp_tool_call.rs:51`、`codex-rs/core/src/mcp_tool_call.rs:66` |
| 计划更新 | `PlanUpdate` | `codex-rs/core/src/tools/handlers/plan.rs:108` |

### 2.8 你当前配置（`never` + `danger-full-access`）会让流程变成什么样

> 这里不做“概念推测”，只写源码里明确的分支行为。

- 审批：`AskForApproval::Never` 会让默认执行审批变为 `Skip`（不需要 user approval）。见 `codex-rs/core/src/tools/sandboxing.rs:129`。
- 沙箱：`SandboxPolicy::DangerFullAccess` 在 `SandboxablePreference::Auto` 时会选择 `SandboxType::None`（即不启用平台沙箱包装）。见 `codex-rs/core/src/sandboxing/mod.rs:63` 的 `select_initial`。

---

## 3. 一个最小的“心智模型图”（只保留闭环骨架）

```text
run_task (codex-rs/core/src/codex.rs:2207)
  loop:
    run_turn (codex-rs/core/src/codex.rs:2367)
      try_run_turn: 消费模型流 (codex-rs/core/src/codex.rs:2506)
        OutputItemDone -> handle_output_item_done (codex-rs/core/src/stream_events_utils.rs:43)
          如果是 ToolCall:
            记录到历史 (record_conversation_items)
            启动工具 future (ToolCallRuntime)
            needs_follow_up = true
          否则:
            当作普通消息/推理入历史
      drain_in_flight: 等所有工具结束并把输出入历史 (codex-rs/core/src/codex.rs:2702)
    如果 needs_follow_up = false: break (任务收敛)
```

---

## 4. 本轮对话记录（原文整理）

> 说明：只保留“用户/助手”的自然语言对话与示例代码，不包含我在后台用来定位源码的命令输出（避免文档过度膨胀）。

### 4.0 用户（上下文/环境）

用户提供的 `AGENTS.md` 指令片段：

```text
# AGENTS.md instructions for /mnt/d/pos-src/codex

<INSTRUCTIONS>
你是一个具有最前沿知识的架构师、产品经理、程序员。
所有回复都必须是中文，注释也必须中文；
所有回复都必须基于对当前项目源代码的理解之上，不许臆造； 
如果要进行方案设计，先联网搜索最新最流行的方案，再结合自己的理解提供最佳方案给我评审；
如果修改源代码，那么需要先尽可能大范围的理解全部相关源码，然后给出最佳修改的方案让我评审，我评审后再进行修改；
改完后不用测试可编译性，我自己会操作；

--- project-doc ---

# Rust/codex-rs

In the codex-rs folder where the rust code lives:

- Crate names are prefixed with `codex-`. For example, the `core` folder's crate is named `codex-core`
- When using format! and you can inline variables into {}, always do that.
- Install any commands the repo relies on (for example `just`, `rg`, or `cargo-insta`) if they aren't already available before running instructions here.
- Never add or modify any code related to `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` or `CODEX_SANDBOX_ENV_VAR`.
  - You operate in a sandbox where `CODEX_SANDBOX_NETWORK_DISABLED=1` will be set whenever you use the `shell` tool. Any existing code that uses `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` was authored with this fact in mind. It is often used to early exit out of tests that the author knew you would not be able to run given your sandbox limitations.
  - Similarly, when you spawn a process using Seatbelt (`/usr/bin/sandbox-exec`), `CODEX_SANDBOX=seatbelt` will be set on the child process. Integration tests that want to run Seatbelt themselves cannot be run under Seatbelt, so checks for `CODEX_SANDBOX=seatbelt` are also often used to early exit out of tests, as appropriate.
- Always collapse if statements per https://rust-lang.github.io/rust-clippy/master/index.html#collapsible_if
- Always inline format! args when possible per https://rust-lang.github.io/rust-clippy/master/index.html#uninlined_format_args
- Use method references over closures when possible per https://rust-lang.github.io/rust-clippy/master/index.html#redundant_closure_for_method_calls
- When writing tests, prefer comparing the equality of entire objects over fields one by one.
- When making a change that adds or changes an API, ensure that the documentation in the `docs/` folder is up to date if applicable.

Run `just fmt` (in `codex-rs` directory) automatically after making Rust code changes; do not ask for approval to run it. Before finalizing a change to `codex-rs`, run `just fix -p <project>` (in `codex-rs` directory) to fix any linter issues in the code. Prefer scoping with `-p` to avoid slow workspace‑wide Clippy builds; only run `just fix` without `-p` if you changed shared crates. Additionally, run the tests:

1. Run the test for the specific project that was changed. For example, if changes were made in `codex-rs/tui`, run `cargo test -p codex-tui`.
2. Once those pass, if any changes were made in common, core, or protocol, run the complete test suite with `cargo test --all-features`.
   When running interactively, ask the user before running `just fix` to finalize. `just fmt` does not require approval. project-specific or individual tests can be run without asking the user, but do ask the user before running the complete test suite.

## TUI style conventions

See `codex-rs/tui/styles.md`.

## TUI code conventions

- Use concise styling helpers from ratatui’s Stylize trait.
  - Basic spans: use "text".into()
  - Styled spans: use "text".red(), "text".green(), "text".magenta(), "text".dim(), etc.
  - Prefer these over constructing styles with `Span::styled` and `Style` directly.
  - Example: patch summary file lines
    - Desired: vec!["  └ ".into(), "M".red(), " ".dim(), "tui/src/app.rs".dim()]

### TUI Styling (ratatui)

- Prefer Stylize helpers: use "text".dim(), .bold(), .cyan(), .italic(), .underlined() instead of manual Style where possible.
- Prefer simple conversions: use "text".into() for spans and vec![…].into() for lines; when inference is ambiguous (e.g., Paragraph::new/Cell::from), use Line::from(spans) or Span::from(text).
- Computed styles: if the Style is computed at runtime, using `Span::styled` is OK (`Span::from(text).set_style(style)` is also acceptable).
- Avoid hardcoded white: do not use `.white()`; prefer the default foreground (no color).
- Chaining: combine helpers by chaining for readability (e.g., url.cyan().underlined()).
- Single items: prefer "text".into(); use Line::from(text) or Span::from(text) only when the target type isn’t obvious from context, or when using .into() would require extra type annotations.
- Building lines: use vec![…].into() to construct a Line when the target type is obvious and no extra type annotations are needed; otherwise use Line::from(vec![…]).
- Avoid churn: don’t refactor between equivalent forms (Span::styled ↔ set_style, Line::from ↔ .into()) without a clear readability or functional gain; follow file‑local conventions and do not introduce type annotations solely to satisfy .into().
- Compactness: prefer the form that stays on one line after rustfmt; if only one of Line::from(vec![…]) or vec![…].into() avoids wrapping, choose that. If both wrap, pick the one with fewer wrapped lines.

### Text wrapping

- Always use textwrap::wrap to wrap plain strings.
- If you have a ratatui Line and you want to wrap it, use the helpers in tui/src/wrapping.rs, e.g. word_wrap_lines / word_wrap_line.
- If you need to indent wrapped lines, use the initial_indent / subsequent_indent options from RtOptions if you can, rather than writing custom logic.
- If you have a list of lines and you need to prefix them all with some prefix (optionally different on the first vs subsequent lines), use the `prefix_lines` helper from line_utils.

## Tests

### Snapshot tests

This repo uses snapshot tests (via `insta`), especially in `codex-rs/tui`, to validate rendered output. When UI or text output changes intentionally, update the snapshots as follows:

- Run tests to generate any updated snapshots:
  - `cargo test -p codex-tui`
- Check what’s pending:
  - `cargo insta pending-snapshots -p codex-tui`
- Review changes by reading the generated `*.snap.new` files directly in the repo, or preview a specific file:
  - `cargo insta show -p codex-tui path/to/file.snap.new`
- Only if you intend to accept all new snapshots in this crate, run:
  - `cargo insta accept -p codex-tui`

If you don’t have the tool:

- `cargo install cargo-insta`

### Test assertions

- Tests should use pretty_assertions::assert_eq for clearer diffs. Import this at the top of the test module if it isn't already.
- Prefer deep equals comparisons whenever possible. Perform `assert_eq!()` on entire objects, rather than individual fields.
- Avoid mutating process environment in tests; prefer passing environment-derived flags or dependencies from above.

### Spawning workspace binaries in tests (Cargo vs Buck2)

- Prefer `codex_utils_cargo_bin::cargo_bin("...")` over `assert_cmd::Command::cargo_bin(...)` or `escargot` when tests need to spawn first-party binaries.
  - Under Buck2, `CARGO_BIN_EXE_*` may be project-relative (e.g. `buck-out/...`), which breaks if a test changes its working directory. `codex_utils_cargo_bin::cargo_bin` resolves to an absolute path first.
- When locating fixture files under Buck2, avoid `env!("CARGO_MANIFEST_DIR")` (Buck codegen sets it to `"."`). Prefer deriving paths from `codex_utils_cargo_bin::buck_project_root()` when needed.

### Integration tests (core)

- Prefer the utilities in `core_test_support::responses` when writing end-to-end Codex tests.

- All `mount_sse*` helpers return a `ResponseMock`; hold onto it so you can assert against outbound `/responses` POST bodies.
- Use `ResponseMock::single_request()` when a test should only issue one POST, or `ResponseMock::requests()` to inspect every captured `ResponsesRequest`.
- `ResponsesRequest` exposes helpers (`body_json`, `input`, `function_call_output`, `custom_tool_call_output`, `call_output`, `header`, `path`, `query_param`) so assertions can target structured payloads instead of manual JSON digging.
- Build SSE payloads with the provided `ev_*` constructors and the `sse(...)`.
- Prefer `wait_for_event` over `wait_for_event_with_timeout`.
- Prefer `mount_sse_once` over `mount_sse_once_match` or `mount_sse_sequence`

- Typical pattern:

  ```rust
  let mock = responses::mount_sse_once(&server, responses::sse(vec![
      responses::ev_response_created("resp-1"),
      responses::ev_function_call(call_id, "shell", &serde_json::to_string(&args)?),
      responses::ev_completed("resp-1"),
  ])).await;

  codex.submit(Op::UserTurn { ... }).await?;

  // Assert request body if needed.
  let request = mock.single_request();
  // assert using request.function_call_output(call_id) or request.json_body() or other helpers.
  ```

</INSTRUCTIONS>
```

用户提供的环境上下文片段：

```xml
<environment_context>
  <cwd>/mnt/d/pos-src/codex</cwd>
  <approval_policy>on-request</approval_policy>
  <sandbox_mode>workspace-write</sandbox_mode>
  <network_access>enabled</network_access>
  <shell>bash</shell>
</environment_context>

<environment_context>
  <approval_policy>never</approval_policy>
  <sandbox_mode>danger-full-access</sandbox_mode>
  <network_access>enabled</network_access>
  <shell>bash</shell>
</environment_context>
```

### 4.1 用户

我想知道codex-cli的工具集成和迭代机制，这点是codex-cli能独立完成真实工程任务的主要原因，目的是要让我真正理解它的Agentic

### 4.2 助手

Codex CLI 之所以“Agentic”（能独立推进真实工程任务），核心不是“模型更聪明”，而是 `codex-core` 把模型放进了一个**可行动、可观测、可回路迭代**的运行时里：模型→调用工具→拿到真实环境反馈→再调用模型，直到收敛。

**1）工具集成：工具是怎么“接到模型上”的**

- **每一轮都会把工具清单作为 schema 发给模型**：在 `codex-rs/core/src/codex.rs:2367` 的 `run_turn` 里构造 `Prompt { tools: router.specs(), parallel_tool_calls: ... }`，工具列表来自 `ToolRouter::from_config(...)`。
- **工具定义（schema）由本地注册表生成 + MCP 动态补充**：`codex-rs/core/src/tools/spec.rs:979` 的 `build_specs` 负责把内置工具（`shell` / `apply_patch` / `read_file` / `list_dir` / `update_plan` / MCP resource 工具等）注册成 `ToolSpec`，并绑定到具体 `ToolHandler`。
- **模型发来的 tool call 会被统一解析成 ToolCall**：`codex-rs/core/src/tools/router.rs:59` 的 `build_tool_call` 把 `ResponseItem::FunctionCall / CustomToolCall / LocalShellCall` 解析成内部 `ToolCall`（并识别 MCP 工具）。
- **分发执行：tool_name → handler**：`codex-rs/core/src/tools/registry.rs:61` 的 `ToolRegistry::dispatch` 根据名字找到 handler，执行后把结果包装成 `ResponseInputItem`（即“工具输出消息”）回写进对话历史，供下一轮模型读取。
- **可扩展：MCP 是“外插工具总线”**：`codex-rs/core/src/codex.rs:2374` 每轮都会从 `McpConnectionManager` 拉取所有 MCP tools 并注入工具清单；调用时走 `codex-rs/core/src/tools/handlers/mcp.rs` → `codex-rs/core/src/mcp_tool_call.rs`，所以你可以把数据库、内部系统、云 API 等能力以 MCP server 的形式接进来，而不需要改 Codex 核心。

**2）迭代机制：它是怎么“跑起来直到完成”的**

- **外层是一个“任务循环”**：`codex-rs/core/src/codex.rs:2207` 的 `run_task` 不是调用一次模型就结束，而是不断构造“当前历史 + 最新工具输出”的 `turn_input`，然后调用 `run_turn`。
- **内层是“流式 turn 执行 + 工具并发”**：`codex-rs/core/src/codex.rs:2506` 的 `try_run_turn` 处理模型流式输出；当某个输出项完成时，会在 `codex-rs/core/src/stream_events_utils.rs:43` 的 `handle_output_item_done` 判断“这是不是 tool call”：
  - 是 tool call：立即把该 item 记入历史，并通过 `ToolCallRuntime` 启动工具执行（`codex-rs/core/src/tools/parallel.rs:49`）。
  - 不是 tool call：当作普通消息/推理项结束，记入历史。
- **“needs_follow_up” 是迭代开关**：只要本轮出现过 tool call，就会 `needs_follow_up = true`，`run_task` 继续下一轮，把工具输出带回模型，直到某一轮没有 tool call（任务收敛）。
- **并发机制是“工具声明 + 运行时锁”**：哪些工具允许并发由 registry 标记；运行时在 `codex-rs/core/src/tools/parallel.rs` 用读写锁实现——支持并发的工具并行跑，不支持的工具串行，减少互相踩踏（尤其对写操作/交互式执行很重要）。

**3）为什么它能做“真实工程”，而不只是聊天**

- **真实反馈闭环**：`shell`/`exec_command` 能跑编译、测试、脚本，把“环境真实输出”变成下一轮的输入（不是靠猜）。
- **安全与可控**：工具执行统一走 `ToolOrchestrator`（`codex-rs/core/src/tools/orchestrator.rs:34`），先决策是否需要审批，再选择 sandbox，必要时按策略重试；并且对“变更类工具”有额外门禁——`ToolRegistry` 会对 mutating 工具 `wait_ready`（`codex-rs/core/src/tools/registry.rs:115`），而 `GhostSnapshotTask` 会在后台先做快照再放行（`codex-rs/core/src/codex.rs:1447` + `codex-rs/core/src/tasks/ghost_snapshot.rs:149`），保证可撤销/可审计。
- **可观测的执行轨迹**：所有关键动作都会发事件（命令开始/结束、打补丁开始/结束、MCP 调用等），`codex exec --json` 还能把这些事件流式输出成 JSONL（见 `docs/exec.md`），这让“自动化”从黑盒变成可追踪过程。

如果你愿意，我可以按“从一次真实交互出发”带你走一遍：给一个典型任务（比如“改一个 bug 并跑测试”），我把它在 `run_task/run_turn/ToolRouter/ToolOrchestrator` 里对应到每一步事件和数据结构（`ResponseItem/ResponseInputItem/TurnItem`），你会更直观看到 Agentic 回路是怎么闭合的。

### 4.3 用户

可以

### 4.4 用户

可以

### 4.5 助手

好，我们用一个典型工程任务走一遍：“先用命令确认现象 → 改代码 → 再跑命令验证 → 给总结”。下面把 Codex CLI 的 **控制流 / 数据流 / 事件流** 对齐到源码（不假设模型一定这么做，只给你可复现的机制骨架）。

**1）一次任务（task）里如何“反复迭代”**

- 外层循环在 `codex-rs/core/src/codex.rs:2207` 的 `run_task`：
  - 准备用户输入并写入历史：`record_response_item_and_emit_turn_item`（`codex-rs/core/src/codex.rs:1394` 被调用于 `codex-rs/core/src/codex.rs:2249`）。
  - 每一轮 turn 都用 `sess.clone_history().await.get_history_for_prompt()` 组装要发给模型的 `turn_input`（`codex-rs/core/src/codex.rs:2272` 附近）。
  - 调 `run_turn(...)` 发起一次模型流式调用：`codex-rs/core/src/codex.rs:2367`。
  - 只要该 turn 里出现过工具调用，`needs_follow_up` 就会变成 `true`，`run_task` 继续下一轮，把工具输出带回模型；反之（没有工具调用）就视为任务收敛，退出循环：`codex-rs/core/src/codex.rs:2313`。

直觉理解：`run_task` 就是一个“直到不再需要工具”为止的 while 循环。

**2）一次 turn 内部：模型输出是“流式事件”，工具执行是“并发 future”**

- turn 的核心在 `try_run_turn`：`codex-rs/core/src/codex.rs:2506`。
- `ModelClient::stream(prompt)` 返回一个 `stream`，Codex 逐个 `next()` 处理 `ResponseEvent`。
- 当遇到 `ResponseEvent::OutputItemDone(item)` 时，会调用 `handle_output_item_done`：`codex-rs/core/src/codex.rs:2587` → `codex-rs/core/src/stream_events_utils.rs:43`。
  - 如果 `item` 是工具调用：
    - `ToolRouter::build_tool_call(...)` 解析出内部 `ToolCall`：`codex-rs/core/src/tools/router.rs:59`
    - 立刻把该输出项写入历史（确保历史/rollout同步）：`record_conversation_items`
    - 通过 `ToolCallRuntime::handle_tool_call` 启动工具执行 future：`codex-rs/core/src/tools/parallel.rs:49`
    - 设置 `needs_follow_up = true`
  - 如果 `item` 不是工具调用：
    - 会被转换成 UI 需要的 `TurnItem` 并发 started/completed 事件：`handle_non_tool_response_item`（`codex-rs/core/src/stream_events_utils.rs:101`）→ `parse_turn_item`（`codex-rs/core/src/event_mapping.rs:71`）
    - 同样写入历史

- 关键点：工具 future 不一定立刻完成，所以 `try_run_turn` 维护了一个 `in_flight: FuturesOrdered` 队列（`codex-rs/core/src/codex.rs:2542`）。
- 当 `ResponseEvent::Completed` 到来后，仍然会 `drain_in_flight(...)` 等所有工具 future 收尾，并把工具输出写回历史：`codex-rs/core/src/codex.rs:2702`。

直觉理解：一轮 turn 里，模型可以一边继续输出，工具也可以一边在后台跑；但在 turn 结束前，Codex 会把所有工具输出“排干净”，保证下一轮一定能看到真实结果。

**3）工具链：从 “tool call” 到 “执行输出” 的路径**

以 `shell` 为例（`apply_patch` 类似）：

- 模型发出 tool call（FunctionCall/LocalShellCall 等）→ `ToolRouter::build_tool_call`：`codex-rs/core/src/tools/router.rs:59`
- 分发调用 → `ToolRouter::dispatch_tool_call`：`codex-rs/core/src/tools/router.rs:130`
- 具体执行 → `ToolRegistry::dispatch` 找 handler：`codex-rs/core/src/tools/registry.rs:61`
- `ShellHandler::handle` 解析参数并进入 `run_exec_like`：`codex-rs/core/src/tools/handlers/shell.rs:203`
- `run_exec_like` 用 `ToolOrchestrator` 做统一的：审批 → 沙箱选择 → 执行/必要时重试：`codex-rs/core/src/tools/orchestrator.rs:34`
- 执行过程中会 emit 事件（给 TUI/exec json 订阅者）：`ExecCommandBegin/End` 在 `codex-rs/core/src/tools/events.rs`

同样，`apply_patch` 会先校验补丁语法/正确性（`maybe_parse_apply_patch_verified`），并在必要时委托给 exec，以统一平台行为：`codex-rs/core/src/tools/handlers/apply_patch.rs`。

**4）为什么它敢“自动改代码”：变更门禁与 turn diff**

- `ToolRegistry::dispatch` 会在 mutating 工具执行前等待 gate：`codex-rs/core/src/tools/registry.rs:115`。
- gate 的一个用途是让 Ghost Snapshot 先完成（可撤销/undo 的基础）：`codex-rs/core/src/codex.rs:1439` 启动后台任务，任务结束时 `mark_ready`：`codex-rs/core/src/tasks/ghost_snapshot.rs:149`。
- 每次 apply_patch 开始会调用 `TurnDiffTracker::on_patch_begin` 记录基线，turn 结束时会聚合 diff 并发 `TurnDiff`：`codex-rs/core/src/codex.rs:2704` 与 `codex-rs/core/src/turn_diff_tracker.rs:54`。

如果你希望我把这个“典型任务”再具体化成一个可跑的例子（比如：让模型先 `rg` 找一个 TODO，然后 `apply_patch` 改掉，再 `cargo test` 验证），我可以按同样的路径把每一步会产生哪些事件、会写入哪些 `ResponseItem` 也列出来。

### 4.6 用户

理解codex-cli源码，对于学习agent开发有帮助吗，我是一个agent小白，我必须理解codex-cli的运行原理，才能学到东西，而不是停留在概念上

### 4.7 助手

有帮助，而且对“小白”最有价值的点在于：它不是讲概念，而是给你一套**可运行的 Agent 运行时**（循环、工具、反馈、收敛、安全）。但你不需要“全读完”，否则很容易被工程细节淹没；真正必须吃透的是这条主链路。

- **最值得学的 3 条主线（对应源码入口）**
  - 任务循环（Agent 迭代收敛）：`codex-rs/core/src/codex.rs:2207` 的 `run_task`（直到 `needs_follow_up=false` 才结束）
  - 工具调用闭环（模型→工具→输出→再喂回模型）：`codex-rs/core/src/stream_events_utils.rs:43` → `codex-rs/core/src/tools/router.rs:59` → `codex-rs/core/src/tools/registry.rs:61`
  - 执行编排（审批/沙箱/重试/门禁）：`codex-rs/core/src/tools/orchestrator.rs:34` + `codex-rs/core/src/tools/sandboxing.rs:129` + mutating 门禁 `codex-rs/core/src/tools/registry.rs:115`

- **给小白的“学到东西”的方式（不停留概念）**
  - 先只跟一条最小闭环：读 `run_task` → `run_turn`（`codex-rs/core/src/codex.rs:2367`）→ `handle_output_item_done`，把“为什么会继续下一轮”完全想明白。
  - 再选两个工具当样本：`shell`（`codex-rs/core/src/tools/handlers/shell.rs`）+ `apply_patch`（`codex-rs/core/src/tools/handlers/apply_patch.rs`），看它们如何把真实执行结果变成下一轮输入。
  - 最后再补“工程级能力”：并发/串行（`codex-rs/core/src/tools/parallel.rs:49`）、diff 聚合（`codex-rs/core/src/turn_diff_tracker.rs:54`）、MCP 外接工具（`codex-rs/core/src/mcp_tool_call.rs`）。

如果你愿意，我可以按“你自己写一个 200 行的最小 Agent”来带你：每一步都用 Codex CLI 里对应的函数/结构做映射，这样你会在动手过程中真正理解它的运行原理。你更想用 Rust 还是 Python 来写这个最小 Agent？

### 4.8 用户

用rust，要有注释，所谓的run_turn，handle_output_item_done从字面意思我不理解它们是干嘛的

### 4.9 助手

**`run_turn` 是干嘛的（为什么叫 turn）**

- 在 Codex 里，一个 *turn* 基本等价于“向模型发起一次请求 + 处理这次响应流直到 `Completed`”。
- 代码位置：`codex-rs/core/src/codex.rs:2367`

**`handle_output_item_done` 是干嘛的（为什么叫 output item）**

- 模型是“流式”输出的：会先出现“某个输出项开始/持续 delta”，最后出现“这个输出项完成（done）”。
- 代码位置：`codex-rs/core/src/stream_events_utils.rs:43`

并给出一份“教学版最小 Agent（Rust 单文件，注释中文）”：

```rust
// 说明：这段代码是教学版骨架，用来把 run_task/run_turn/handle_output_item_done 的职责讲清楚。
// 它不是本仓库代码的一部分，也不是对 Codex 的一比一拷贝实现。

use std::collections::HashMap;

#[derive(Debug, Clone)]
enum ResponseItem {
    User(String),
    Assistant(String),
    ToolCall { call_id: String, name: String, args: ToolArgs },
    ToolOutput { call_id: String, content: String },
}

#[derive(Debug, Clone)]
enum ToolArgs {
    Shell { command: String },
    ApplyPatch { path: String, content: String },
}

#[derive(Debug, Clone)]
struct ToolCall {
    tool_name: String,
    call_id: String,
    args: ToolArgs,
}

struct Session {
    history: Vec<ResponseItem>,
    files: HashMap<String, String>,
}

impl Session {
    fn new() -> Self {
        Self { history: Vec::new(), files: HashMap::new() }
    }
    fn record(&mut self, item: ResponseItem) {
        self.history.push(item);
    }
}

trait ToolHandler {
    fn handle(&self, session: &mut Session, call: &ToolCall) -> String;
}

struct ShellTool;
impl ToolHandler for ShellTool {
    fn handle(&self, session: &mut Session, call: &ToolCall) -> String {
        let ToolArgs::Shell { command } = &call.args else {
            return "Exit code: 1\nWall time: 0.0 seconds\nOutput:\n参数不匹配".to_string();
        };
        if let Some(path) = command.strip_prefix("cat ").map(str::trim).filter(|s| !s.is_empty()) {
            match session.files.get(path) {
                Some(content) => format!("Exit code: 0\nWall time: 0.0 seconds\nOutput:\n{content}"),
                None => format!("Exit code: 1\nWall time: 0.0 seconds\nOutput:\n{path}: No such file"),
            }
        } else {
            "Exit code: 1\nWall time: 0.0 seconds\nOutput:\n教学版只支持 `cat <path>`".to_string()
        }
    }
}

struct ApplyPatchTool;
impl ToolHandler for ApplyPatchTool {
    fn handle(&self, session: &mut Session, call: &ToolCall) -> String {
        let ToolArgs::ApplyPatch { path, content } = &call.args else {
            return "补丁参数不匹配".to_string();
        };
        session.files.insert(path.clone(), content.clone());
        format!("已写入文件：{path}（{len} 字节）", len = content.len())
    }
}

struct ToolRegistry {
    handlers: HashMap<String, Box<dyn ToolHandler>>,
}

impl ToolRegistry {
    fn new() -> Self { Self { handlers: HashMap::new() } }
    fn register(mut self, name: &str, handler: impl ToolHandler + 'static) -> Self {
        self.handlers.insert(name.to_string(), Box::new(handler));
        self
    }
    fn dispatch(&self, session: &mut Session, call: &ToolCall) -> String {
        match self.handlers.get(&call.tool_name) {
            Some(h) => h.handle(session, call),
            None => format!("不支持的工具：{}", call.tool_name),
        }
    }
}

struct ToolRouter;
impl ToolRouter {
    fn build_tool_call(item: &ResponseItem) -> Option<ToolCall> {
        match item {
            ResponseItem::ToolCall { call_id, name, args } => Some(ToolCall {
                tool_name: name.clone(),
                call_id: call_id.clone(),
                args: args.clone(),
            }),
            _ => None,
        }
    }
}

#[derive(Default)]
struct OutputItemResult {
    last_agent_message: Option<String>,
    needs_follow_up: bool,
}

fn handle_output_item_done(session: &mut Session, tools: &ToolRegistry, item: ResponseItem) -> OutputItemResult {
    session.record(item.clone());
    if let Some(call) = ToolRouter::build_tool_call(&item) {
        let tool_output_text = tools.dispatch(session, &call);
        session.record(ResponseItem::ToolOutput { call_id: call.call_id.clone(), content: tool_output_text });
        return OutputItemResult { needs_follow_up: true, last_agent_message: None };
    }
    match item {
        ResponseItem::Assistant(text) => OutputItemResult { needs_follow_up: false, last_agent_message: Some(text) },
        _ => OutputItemResult { needs_follow_up: false, last_agent_message: None },
    }
}

struct TurnRunResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
}

fn run_turn(session: &mut Session, model: &mut FakeModel, tools: &ToolRegistry) -> TurnRunResult {
    let items = model.produce_items();
    let mut needs_follow_up = false;
    let mut last_agent_message = None;
    for item in items {
        let out = handle_output_item_done(session, tools, item);
        needs_follow_up |= out.needs_follow_up;
        if out.last_agent_message.is_some() {
            last_agent_message = out.last_agent_message;
        }
    }
    TurnRunResult { needs_follow_up, last_agent_message }
}

struct FakeModel { stage: u8 }
impl FakeModel {
    fn new() -> Self { Self { stage: 0 } }
    fn produce_items(&mut self) -> Vec<ResponseItem> {
        let items = match self.stage {
            0 => vec![ResponseItem::ToolCall { call_id: "call-1".to_string(), name: "shell".to_string(), args: ToolArgs::Shell { command: "cat hello.txt".to_string() } }],
            1 => vec![ResponseItem::ToolCall { call_id: "call-2".to_string(), name: "apply_patch".to_string(), args: ToolArgs::ApplyPatch { path: "hello.txt".to_string(), content: "你好，Agent！\n".to_string() } }],
            2 => vec![ResponseItem::ToolCall { call_id: "call-3".to_string(), name: "shell".to_string(), args: ToolArgs::Shell { command: "cat hello.txt".to_string() } }],
            _ => vec![ResponseItem::Assistant("完成：文件已写入并验证读取成功。".to_string())],
        };
        self.stage = self.stage.saturating_add(1);
        items
    }
}

fn run_task(session: &mut Session, model: &mut FakeModel, tools: &ToolRegistry) -> Option<String> {
    loop {
        let result = run_turn(session, model, tools);
        if !result.needs_follow_up {
            return result.last_agent_message;
        }
    }
}

fn main() {
    let tools = ToolRegistry::new()
        .register("shell", ShellTool)
        .register("apply_patch", ApplyPatchTool);

    let mut session = Session::new();
    session.record(ResponseItem::User("请创建 hello.txt 并验证内容".to_string()));

    let mut model = FakeModel::new();
    let final_msg = run_task(&mut session, &mut model, &tools);

    println!("最终输出：{final_msg:?}");
    println!("历史条目数：{}", session.history.len());
}
```

如果你看完这段还卡在“为什么要区分 `run_turn` 和 `handle_output_item_done`”，我建议你下一步只盯一处源码看：`codex-rs/core/src/codex.rs:2580` 的 `match ResponseEvent`——它会让你直观看到：**流里每来一个“输出项完成”，就调用一次 `handle_output_item_done`**，而 `run_turn` 负责把这一整轮流消费完并汇总结果。

你希望我接下来把上面教学版的每一行，对应映射回 Codex 的真实实现点（`ResponseEvent::OutputItemAdded/Done/Completed`、in_flight、drain、TurnDiff）做一份“对照表”吗？

### 4.10 用户

可以，完成对照后，把这轮对话内容写入一个markdown文档，文档名要中文

### 4.11 助手

已完成对照表，并把本轮对话内容写入本文档：`本轮对话记录与Codex源码对照.md`。
