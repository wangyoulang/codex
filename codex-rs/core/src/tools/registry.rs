use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::client_common::tools::ToolSpec;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use async_trait::async_trait;
use codex_protocol::models::ResponseInputItem;
use codex_utils_readiness::Readiness;
use tracing::warn;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolKind {
    Function,
    Mcp,
}

#[async_trait]
pub trait ToolHandler: Send + Sync {
    fn kind(&self) -> ToolKind;

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            (self.kind(), payload),
            (ToolKind::Function, ToolPayload::Function { .. })
                | (ToolKind::Mcp, ToolPayload::Mcp { .. })
        )
    }

    async fn is_mutating(&self, _invocation: &ToolInvocation) -> bool {
        false
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError>;
}

pub struct ToolRegistry {
    handlers: HashMap<String, Arc<dyn ToolHandler>>,
}

impl ToolRegistry {
    pub fn new(handlers: HashMap<String, Arc<dyn ToolHandler>>) -> Self {
        Self { handlers }
    }

    pub fn handler(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.handlers.get(name).map(Arc::clone)
    }

    // TODO(jif) for dynamic tools.
    // pub fn register(&mut self, name: impl Into<String>, handler: Arc<dyn ToolHandler>) {
    //     let name = name.into();
    //     if self.handlers.insert(name.clone(), handler).is_some() {
    //         warn!("overwriting handler for tool {name}");
    //     }
    // }

    pub async fn dispatch(
        &self,
        invocation: ToolInvocation,
    ) -> Result<ResponseInputItem, FunctionCallError> {
        // 这是“工具总入口”的核心调度方法：给定一次 ToolInvocation（一次具体的工具调用上下文），
        // 找到对应的 ToolHandler 执行，并把执行结果转换成 `ResponseInputItem` 返回给上层。
        //
        // 输入（ToolInvocation）里包含：
        // - tool_name / call_id / payload：模型发起的调用信息（调用哪个工具、该次调用的 id、参数载荷）；
        // - session / turn / tracker：工具执行所需的上下文（会话状态、turn 约束、diff 跟踪器）。
        //
        // 输出（ResponseInputItem）是“可写回对话历史/回写给模型”的标准协议 item：
        // - 成功时：由 `ToolOutput::into_response(...)` 根据 payload 类型（Function/Custom/MCP 等）构造对应的 output item；
        // - 失败时：返回 `FunctionCallError`，由上层决定是中断（Fatal）还是转成失败输出回写给模型（RespondToModel/Denied 等）。
        //
        // 这层还负责把每次工具调用的可观测性信息（tool_name/call_id/payload/耗时/成功与否）打到 OTel。
        let tool_name = invocation.tool_name.clone();
        let call_id_owned = invocation.call_id.clone();
        let otel = invocation.turn.client.get_otel_manager();
        let payload_for_response = invocation.payload.clone();
        let log_payload = payload_for_response.log_payload();

        let handler = match self.handler(tool_name.as_ref()) {
            Some(handler) => handler,
            None => {
                // 没注册这个 tool_name：这通常意味着
                // - 模型调用了一个不存在/未启用的工具；或
                // - 配置/feature 没把某个工具注册进来。
                // 这里选择“提示模型改正”（RespondToModel），而不是直接 Fatal。
                let message =
                    unsupported_tool_call_message(&invocation.payload, tool_name.as_ref());
                otel.tool_result(
                    tool_name.as_ref(),
                    &call_id_owned,
                    log_payload.as_ref(),
                    Duration::ZERO,
                    false,
                    &message,
                );
                return Err(FunctionCallError::RespondToModel(message));
            }
        };

        if !handler.matches_kind(&invocation.payload) {
            // 同名工具但 payload 类型不匹配（例如 Function 工具拿到了 Mcp payload）。
            // 这属于内部不变量被破坏：继续执行可能产生不可预期行为，因此用 Fatal 直接上抛。
            let message = format!("tool {tool_name} invoked with incompatible payload");
            otel.tool_result(
                tool_name.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                Duration::ZERO,
                false,
                &message,
            );
            return Err(FunctionCallError::Fatal(message));
        }

        // `log_tool_result(...)` 的闭包只允许我们返回“用于日志的预览文本 + success 标志”，
        // 但上层真正需要的是完整 `ToolOutput`，以便构造 `ResponseInputItem` 写回模型。
        // 所以这里用一个 `Mutex<Option<ToolOutput>>` 在闭包内“顺手存下来”，闭包执行完再取出返回。
        let output_cell = tokio::sync::Mutex::new(None);

        let result = otel
            .log_tool_result(
                tool_name.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                // 工具真正执行的方法，是一个匿名方法，没有参数
                || {
                    let handler = handler.clone();
                    let output_cell = &output_cell;
                    let invocation = invocation;
                    async move {
                        // 如果工具会产生副作用（改文件/跑命令等），需要等 tool_call_gate 放行。
                        // 这用于在某些场景下（例如 UI 正在等待用户审批/或需要先准备环境）阻塞“变更性工具”的执行。
                        if handler.is_mutating(&invocation).await {
                            tracing::trace!("waiting for tool gate");
                            invocation.turn.tool_call_gate.wait_ready().await;
                            tracing::trace!("tool gate released");
                        }
                        match handler.handle(invocation).await {
                            Ok(output) => {
                                // 成功：生成日志预览与 success 标志，并把完整 output 存进 output_cell 供外层取用。
                                let preview = output.log_preview();
                                let success = output.success_for_logging();
                                // log_tool_result 的闭包只允许返回日志预览和 success 标志，
                                // 无法直接把完整输出传回外层，所以闭包里写入output_cell，
                                // log_tool_result 返回后，外层再从 output_cell 取出 ToolOutput 用于构造ResponseInputItem
                                let mut guard = output_cell.lock().await;
                                *guard = Some(output);
                                Ok((preview, success))
                            }
                            Err(err) => Err(err),
                        }
                    }
                },
            )
            .await;

        match result {
            Ok(_) => {
                // `log_tool_result` 成功返回并不意味着我们拿到了 output（理论上闭包必须写入 output_cell）。
                // 若这里取不到 output，说明 handler/闭包逻辑违反约定，属于致命错误。
                let mut guard = output_cell.lock().await;
                let output = guard.take().ok_or_else(|| {
                    FunctionCallError::Fatal("tool produced no output".to_string())
                })?;
                // 把工具输出转换成协议层 ResponseInputItem：
                // - call_id 用于把输出关联回模型那次调用；
                // - payload_for_response 用于决定输出类型（FunctionCallOutput vs CustomToolCallOutput 等）。
                Ok(output.into_response(&call_id_owned, &payload_for_response))
            }
            Err(err) => Err(err),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfiguredToolSpec {
    pub spec: ToolSpec,
    pub supports_parallel_tool_calls: bool,
}

impl ConfiguredToolSpec {
    pub fn new(spec: ToolSpec, supports_parallel_tool_calls: bool) -> Self {
        Self {
            spec,
            supports_parallel_tool_calls,
        }
    }
}

pub struct ToolRegistryBuilder {
    handlers: HashMap<String, Arc<dyn ToolHandler>>,
    specs: Vec<ConfiguredToolSpec>,
}

impl ToolRegistryBuilder {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            specs: Vec::new(),
        }
    }

    pub fn push_spec(&mut self, spec: ToolSpec) {
        self.push_spec_with_parallel_support(spec, false);
    }

    pub fn push_spec_with_parallel_support(
        &mut self,
        spec: ToolSpec,
        supports_parallel_tool_calls: bool,
    ) {
        self.specs
            .push(ConfiguredToolSpec::new(spec, supports_parallel_tool_calls));
    }

    pub fn register_handler(&mut self, name: impl Into<String>, handler: Arc<dyn ToolHandler>) {
        let name = name.into();
        if self
            .handlers
            .insert(name.clone(), handler.clone())
            .is_some()
        {
            warn!("overwriting handler for tool {name}");
        }
    }

    // TODO(jif) for dynamic tools.
    // pub fn register_many<I>(&mut self, names: I, handler: Arc<dyn ToolHandler>)
    // where
    //     I: IntoIterator,
    //     I::Item: Into<String>,
    // {
    //     for name in names {
    //         let name = name.into();
    //         if self
    //             .handlers
    //             .insert(name.clone(), handler.clone())
    //             .is_some()
    //         {
    //             warn!("overwriting handler for tool {name}");
    //         }
    //     }
    // }

    pub fn build(self) -> (Vec<ConfiguredToolSpec>, ToolRegistry) {
        let registry = ToolRegistry::new(self.handlers);
        (self.specs, registry)
    }
}

fn unsupported_tool_call_message(payload: &ToolPayload, tool_name: &str) -> String {
    match payload {
        ToolPayload::Custom { .. } => format!("unsupported custom tool call: {tool_name}"),
        _ => format!("unsupported call: {tool_name}"),
    }
}
