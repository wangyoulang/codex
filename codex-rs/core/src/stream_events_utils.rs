use std::pin::Pin;
use std::sync::Arc;

use codex_protocol::items::TurnItem;
use tokio_util::sync::CancellationToken;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::error::CodexErr;
use crate::error::Result;
use crate::function_tool::FunctionCallError;
use crate::parse_turn_item;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolRouter;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use futures::Future;
use tracing::debug;
use tracing::instrument;

/// Handle a completed output item from the model stream, recording it and
/// queuing any tool execution futures. This records items immediately so
/// history and rollout stay in sync even if the turn is later cancelled.
pub(crate) type InFlightFuture<'f> =
    Pin<Box<dyn Future<Output = Result<ResponseInputItem>> + Send + 'f>>;

#[derive(Default)]
pub(crate) struct OutputItemResult {
    pub last_agent_message: Option<String>,
    pub needs_follow_up: bool,
    pub tool_future: Option<InFlightFuture<'static>>,
}

pub(crate) struct HandleOutputCtx {
    pub sess: Arc<Session>,
    pub turn_context: Arc<TurnContext>,
    pub tool_runtime: ToolCallRuntime,
    pub cancellation_token: CancellationToken,
}

#[instrument(level = "trace", skip_all)]
/// 处理“模型流里已完成的一个 output item”（`ResponseEvent::OutputItemDone` 的 payload）。
///
/// 这一步发生在 core 的 `try_run_turn` 事件循环里：模型每结束一个 output item，就会调用本函数来“收尾”。
/// 这里的目标不是 UI 渲染，而是把 item 正确地：
/// - 写入对话历史（conversation history / rollout），保证可恢复/可回放；
/// - 触发并排队工具调用（tool call），并把“工具输出需要回写给模型”的信号上报给上层；
/// - 必要时补发 turn item started/completed 事件给前端（用于 UI 展示一条完整的消息/推理项）。
///
/// 参数说明：
/// - `ctx`：本轮处理上下文，包含 session、turn_context、工具运行时以及取消令牌；
/// - `item`：模型输出的一个完整 `ResponseItem`（可能是消息/推理/websearch/工具调用等）；
/// - `previously_active_item`：上层在处理 `OutputItemAdded` 时记录的“正在活跃的 TurnItem”。
///   - 如果为 `Some`：说明这个 output item 的 started 事件已经发过了（有 active item 在跟踪 delta）；
///   - 如果为 `None`：说明没有收到/没有处理到对应的 OutputItemAdded，需要在这里补发 started，
///     否则 UI 可能只看到 completed 而没有 started。
///
/// 返回值 `OutputItemResult`：
/// - `tool_future`：若该 item 是工具调用，则返回一个“执行工具并产出 ResponseInputItem”的 future，交给上层 drain；
/// - `needs_follow_up`：只要出现工具调用或需要向模型回写（拒绝/报错等），上层就需要继续发起下一轮模型请求；
/// - `last_agent_message`：从非工具的 assistant message item 中提取最后一段文本（用于 run_task 结束时返回）。
pub(crate) async fn handle_output_item_done(
    ctx: &mut HandleOutputCtx,
    item: ResponseItem,
    previously_active_item: Option<TurnItem>,
) -> Result<OutputItemResult> {
    let mut output = OutputItemResult::default();

    match ToolRouter::build_tool_call(ctx.sess.as_ref(), item.clone()).await {
        // 1) 模型输出的是一次“工具调用”：
        //    - 先把 tool call 这条 item 写入历史（即使后续 turn 被取消，也能回放到“模型确实请求过这个工具”）；
        //    - 再把真正的工具执行排队（返回 future 给上层并行/按序 drain）；
        //    - 标记 needs_follow_up，让上层知道还要把工具输出回写给模型并继续下一轮。
        Ok(Some(call)) => {
            let payload_preview = call.payload.log_payload().into_owned();
            tracing::info!("ToolCall: {} {}", call.tool_name, payload_preview);

            // 写入对话历史
            ctx.sess
                .record_conversation_items(&ctx.turn_context, std::slice::from_ref(&item))
                .await;

            // 为工具调用创建一个“子取消令牌”：
            // - turn 级取消会向下传递；
            // - 但每个工具都有自己的 token，便于更细粒度地 abort。
            let cancellation_token = ctx.cancellation_token.child_token();
            let tool_future: InFlightFuture<'static> = Box::pin(
                ctx.tool_runtime
                    .clone()
                    .handle_tool_call(call, cancellation_token),
            );

            output.needs_follow_up = true;
            output.tool_future = Some(tool_future);
        }
        // 2) 不是工具调用：可能是消息/推理/websearch 等“可展示 item”。
        //    做两件事：
        //    - 生成 TurnItem 并给前端发 started/completed（用于 UI 看到一条完整输出）；
        //    - 把原始 ResponseItem 落盘到对话历史。
        Ok(None) => {
            if let Some(turn_item) = handle_non_tool_response_item(&item).await {
                // 如果上层没有记录 previously_active_item，说明本 item 没有触发过 started（例如缺少 OutputItemAdded），
                // 这里补发 started，避免 UI 状态机不完整。
                if previously_active_item.is_none() {
                    ctx.sess
                        .emit_turn_item_started(&ctx.turn_context, &turn_item)
                        .await;
                }

                // item 完成：向 UI 发 completed（此时内容是完整的，不再有 delta）。
                ctx.sess
                    .emit_turn_item_completed(&ctx.turn_context, turn_item)
                    .await;
            }

            // 无论是否能转换成 TurnItem，都需要把原始 ResponseItem 写入历史，作为后续 prompt 的输入与回放依据。
            ctx.sess
                .record_conversation_items(&ctx.turn_context, std::slice::from_ref(&item))
                .await;
            let last_agent_message = last_assistant_message_from_item(&item);

            output.last_agent_message = last_agent_message;
        }
        // 3) 防护：模型发起了 LocalShellCall 但缺少 call_id/id（无法把工具输出关联回模型的那次调用）。
        //    这里选择：
        //    - 记录错误到 telemetry；
        //    - 仍把“原始 tool call item”写入历史；
        //    - 再人工追加一条“FunctionCallOutput（错误内容）”写回历史，让模型在下一轮看到失败原因；
        //    - 标记 needs_follow_up，促使上层继续下一轮把错误反馈给模型。
        Err(FunctionCallError::MissingLocalShellCallId) => {
            let msg = "LocalShellCall without call_id or id";
            ctx.turn_context
                .client
                .get_otel_manager()
                .log_tool_failed("local_shell", msg);
            tracing::error!(msg);

            // 这里 call_id 置空是因为我们无法确定应该关联到哪个 call_id；
            // 目的只是把错误信息插入对话历史，避免模型/用户“无反馈”地卡住。
            let response = ResponseInputItem::FunctionCallOutput {
                call_id: String::new(),
                output: FunctionCallOutputPayload {
                    content: msg.to_string(),
                    ..Default::default()
                },
            };
            ctx.sess
                .record_conversation_items(&ctx.turn_context, std::slice::from_ref(&item))
                .await;
            if let Some(response_item) = response_input_to_response_item(&response) {
                ctx.sess
                    .record_conversation_items(
                        &ctx.turn_context,
                        std::slice::from_ref(&response_item),
                    )
                    .await;
            }

            output.needs_follow_up = true;
        }
        // 4) 工具调用需要“直接回复模型”（例如参数不合法需要提示模型修正），或被策略拒绝。
        //    处理方式与上面类似：把原始 item 写入历史，再追加一条 FunctionCallOutput（内容为 message）。
        Err(FunctionCallError::RespondToModel(message))
        | Err(FunctionCallError::Denied(message)) => {
            let response = ResponseInputItem::FunctionCallOutput {
                call_id: String::new(),
                output: FunctionCallOutputPayload {
                    content: message,
                    ..Default::default()
                },
            };
            ctx.sess
                .record_conversation_items(&ctx.turn_context, std::slice::from_ref(&item))
                .await;
            if let Some(response_item) = response_input_to_response_item(&response) {
                ctx.sess
                    .record_conversation_items(
                        &ctx.turn_context,
                        std::slice::from_ref(&response_item),
                    )
                    .await;
            }

            output.needs_follow_up = true;
        }
        // 5) 致命错误：直接上抛，交给更上层中止本次 turn/task。
        Err(FunctionCallError::Fatal(message)) => {
            return Err(CodexErr::Fatal(message));
        }
    }

    Ok(output)
}

pub(crate) async fn handle_non_tool_response_item(item: &ResponseItem) -> Option<TurnItem> {
    debug!(?item, "Output item");

    match item {
        ResponseItem::Message { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::WebSearchCall { .. } => parse_turn_item(item),
        ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. } => {
            debug!("unexpected tool output from stream");
            None
        }
        _ => None,
    }
}

pub(crate) fn last_assistant_message_from_item(item: &ResponseItem) -> Option<String> {
    if let ResponseItem::Message { role, content, .. } = item
        && role == "assistant"
    {
        return content.iter().rev().find_map(|ci| match ci {
            codex_protocol::models::ContentItem::OutputText { text } => Some(text.clone()),
            _ => None,
        });
    }
    None
}

pub(crate) fn response_input_to_response_item(input: &ResponseInputItem) -> Option<ResponseItem> {
    match input {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            Some(ResponseItem::FunctionCallOutput {
                call_id: call_id.clone(),
                output: output.clone(),
            })
        }
        ResponseInputItem::CustomToolCallOutput { call_id, output } => {
            Some(ResponseItem::CustomToolCallOutput {
                call_id: call_id.clone(),
                output: output.clone(),
            })
        }
        ResponseInputItem::McpToolCallOutput { call_id, result } => {
            let output = match result {
                Ok(call_tool_result) => FunctionCallOutputPayload::from(call_tool_result),
                Err(err) => FunctionCallOutputPayload {
                    content: err.clone(),
                    success: Some(false),
                    ..Default::default()
                },
            };
            Some(ResponseItem::FunctionCallOutput {
                call_id: call_id.clone(),
                output,
            })
        }
        _ => None,
    }
}
