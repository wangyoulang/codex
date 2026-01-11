use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;
use tokio_util::either::Either;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::instrument;
use tracing::trace_span;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::error::CodexErr;
use crate::function_tool::FunctionCallError;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;

#[derive(Clone)]
pub(crate) struct ToolCallRuntime {
    router: Arc<ToolRouter>,
    session: Arc<Session>,
    turn_context: Arc<TurnContext>,
    tracker: SharedTurnDiffTracker,
    parallel_execution: Arc<RwLock<()>>,
}

impl ToolCallRuntime {
    pub(crate) fn new(
        router: Arc<ToolRouter>,
        session: Arc<Session>,
        turn_context: Arc<TurnContext>,
        tracker: SharedTurnDiffTracker,
    ) -> Self {
        Self {
            router,
            session,
            turn_context,
            tracker,
            parallel_execution: Arc::new(RwLock::new(())),
        }
    }

    /// 执行一次工具调用，并把结果包装成 `ResponseInputItem` 供上层写回“对话历史/回写给模型”。
    ///
    /// 这个函数看起来返回的是一个 Future，但内部会立刻 `tokio::spawn` 一个任务去跑工具：
    /// - 这样做的目的是让“收模型流式事件”与“执行工具”可以并行推进；
    /// - 上层（见 `core/src/codex.rs` 的 `try_run_turn`）会把返回的 future 放进 `FuturesOrdered`，
    ///   在 turn 末尾 drain，按序把工具输出写回历史。
    ///
    /// 并行/串行控制（重点）：
    /// - `supports_parallel` 表示“这个工具是否允许与其它工具并行执行”；
    /// - `parallel_execution` 是一个 `RwLock<()>`：
    ///   - 允许并行的工具拿 `read()`，多个读锁可并存；
    ///   - 不允许并行的工具拿 `write()`，写锁会排它，强制与所有其它工具串行。
    ///
    /// 取消语义：
    /// - `cancellation_token` 被触发时，不再等待工具真实完成，而是立刻返回一个“aborted”风格的输出，
    ///   并在 span 上记录 `aborted = true`，便于可观测性排查。
    ///
    /// 错误语义：
    /// - `FunctionCallError::Fatal` 会被提升为 `CodexErr::Fatal`（直接中断更上层流程）；
    /// - 其它错误也会被包装成 `CodexErr::Fatal`，保证不会悄悄吞掉工具失败。
    #[instrument(level = "trace", skip_all, fields(call = ?call))]
    pub(crate) fn handle_tool_call(
        self,
        call: ToolCall,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<ResponseInputItem, CodexErr>> {
        // 这个工具是否允许并行执行（由 ToolRouter 按工具类型/特性判断）。
        let supports_parallel = self.router.tool_supports_parallel(&call.tool_name);

        // 把运行时所需依赖 clone 出来，移动进 spawn 的异步任务里。
        let router = Arc::clone(&self.router);
        let session = Arc::clone(&self.session);
        let turn = Arc::clone(&self.turn_context);
        let tracker = Arc::clone(&self.tracker);
        let lock = Arc::clone(&self.parallel_execution);
        // 用于在取消时生成 “Wall time: X seconds\naborted by user” 这类输出。
        let started = Instant::now();

        // 为这次工具派发创建一个 span：把 tool_name/call_id 等字段挂上去，方便链路追踪与诊断。
        let dispatch_span = trace_span!(
            "dispatch_tool_call",
            otel.name = call.tool_name.as_str(),
            tool_name = call.tool_name.as_str(),
            call_id = call.call_id.as_str(),
            aborted = false,
        );

        // `AbortOnDropHandle`：如果上层把 future 丢弃（drop），这里会自动 abort 掉 tokio 任务，
        // 避免后台工具执行“泄漏”。
        let handle: AbortOnDropHandle<Result<ResponseInputItem, FunctionCallError>> =
            AbortOnDropHandle::new(tokio::spawn(async move {
                // 两路竞争：
                // - 取消令牌先到：直接返回“aborted”输出（不再等待工具实际执行结果）；
                // - 否则执行真正的 dispatch_tool_call。
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        let secs = started.elapsed().as_secs_f32().max(0.1);
                        dispatch_span.record("aborted", true);
                        Ok(Self::aborted_response(&call, secs))
                    },
                    res = async {
                        // 并行/串行调度：允许并行 → 读锁；不允许并行 → 写锁（排它）。
                        // 这里的 guard 被 `_guard` 持有到 dispatch 完成，从而在整个工具执行期间生效。
                        let _guard = if supports_parallel {
                            Either::Left(lock.read().await)
                        } else {
                            Either::Right(lock.write().await)
                        };

                        // 真正执行工具调用：由 ToolRouter 根据 tool_name/payload 选择 handler，
                        // 并把执行结果包装成 ResponseInputItem（例如 FunctionCallOutput/CustomToolCallOutput 等）。
                        router
                            .dispatch_tool_call(session, turn, tracker, call.clone())
                            .instrument(dispatch_span.clone())
                            .await
                    } => res,
                }
            }));

        async move {
            // 把 tokio 任务的结果映射成 core 统一使用的 CodexErr：
            // - JoinError（例如 task panic/被 abort）也会变成 Fatal，避免默默丢失工具输出。
            match handle.await {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(FunctionCallError::Fatal(message))) => Err(CodexErr::Fatal(message)),
                Ok(Err(other)) => Err(CodexErr::Fatal(other.to_string())),
                Err(err) => Err(CodexErr::Fatal(format!(
                    "tool task failed to receive: {err:?}"
                ))),
            }
        }
        .in_current_span()
    }
}

impl ToolCallRuntime {
    fn aborted_response(call: &ToolCall, secs: f32) -> ResponseInputItem {
        match &call.payload {
            ToolPayload::Custom { .. } => ResponseInputItem::CustomToolCallOutput {
                call_id: call.call_id.clone(),
                output: Self::abort_message(call, secs),
            },
            ToolPayload::Mcp { .. } => ResponseInputItem::McpToolCallOutput {
                call_id: call.call_id.clone(),
                result: Err(Self::abort_message(call, secs)),
            },
            _ => ResponseInputItem::FunctionCallOutput {
                call_id: call.call_id.clone(),
                output: FunctionCallOutputPayload {
                    content: Self::abort_message(call, secs),
                    ..Default::default()
                },
            },
        }
    }

    fn abort_message(call: &ToolCall, secs: f32) -> String {
        match call.tool_name.as_str() {
            "shell" | "container.exec" | "local_shell" | "shell_command" | "unified_exec" => {
                format!("Wall time: {secs:.1} seconds\naborted by user")
            }
            _ => format!("aborted by user after {secs:.1}s"),
        }
    }
}
