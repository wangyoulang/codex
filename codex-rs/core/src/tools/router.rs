use crate::client_common::tools::ToolSpec;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ConfiguredToolSpec;
use crate::tools::registry::ToolRegistry;
use crate::tools::spec::ToolsConfig;
use crate::tools::spec::build_specs;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::ShellToolCallParams;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::instrument;

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub tool_name: String,
    pub call_id: String,
    pub payload: ToolPayload,
}

pub struct ToolRouter {
    registry: ToolRegistry,
    specs: Vec<ConfiguredToolSpec>,
}

impl ToolRouter {
    pub fn from_config(
        config: &ToolsConfig,
        mcp_tools: Option<HashMap<String, mcp_types::Tool>>,
    ) -> Self {
        // build_specs 构建工具注册器：根据配置/特性/MCP 工具生成“工具声明 + 处理器”集合，供模型暴露可用工具并在调用时路由到对应 handler。
        let builder = build_specs(config, mcp_tools);
        let (specs, registry) = builder.build();

        Self { registry, specs }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs
            .iter()
            .map(|config| config.spec.clone())
            .collect()
    }

    pub fn tool_supports_parallel(&self, tool_name: &str) -> bool {
        self.specs
            .iter()
            .filter(|config| config.supports_parallel_tool_calls)
            .any(|config| config.spec.name() == tool_name)
    }

    #[instrument(level = "trace", skip_all, err)]
    pub async fn build_tool_call(
        session: &Session,
        item: ResponseItem,
    ) -> Result<Option<ToolCall>, FunctionCallError> {
        match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                if let Some((server, tool)) = session.parse_mcp_tool_name(&name).await {
                    Ok(Some(ToolCall {
                        tool_name: name,
                        call_id,
                        payload: ToolPayload::Mcp {
                            server,
                            tool,
                            raw_arguments: arguments,
                        },
                    }))
                } else {
                    Ok(Some(ToolCall {
                        tool_name: name,
                        call_id,
                        payload: ToolPayload::Function { arguments },
                    }))
                }
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => Ok(Some(ToolCall {
                tool_name: name,
                call_id,
                payload: ToolPayload::Custom { input },
            })),
            ResponseItem::LocalShellCall {
                id,
                call_id,
                action,
                ..
            } => {
                let call_id = call_id
                    .or(id)
                    .ok_or(FunctionCallError::MissingLocalShellCallId)?;

                match action {
                    LocalShellAction::Exec(exec) => {
                        let params = ShellToolCallParams {
                            command: exec.command,
                            workdir: exec.working_directory,
                            timeout_ms: exec.timeout_ms,
                            sandbox_permissions: Some(SandboxPermissions::UseDefault),
                            justification: None,
                        };
                        Ok(Some(ToolCall {
                            tool_name: "local_shell".to_string(),
                            call_id,
                            payload: ToolPayload::LocalShell { params },
                        }))
                    }
                }
            }
            _ => Ok(None),
        }
    }

    #[instrument(level = "trace", skip_all, err)]
    /// 将一次“工具调用请求”路由到具体工具处理器执行，并把结果转换成可回写给模型的 `ResponseInputItem`。
    ///
    /// 你可以把它理解成：**“模型说要调用某个工具” → “我们执行工具” → “把执行结果变成一条 tool output 写回对话”**。
    ///
    /// **输入参数解释：**
    /// - `session: Arc<Session>`：本次会话的全局状态与服务集合（历史记录、事件发送、MCP 工具解析等）。
    ///   工具在执行时经常需要读/写 session 状态（例如记录对话项、访问配置/服务）。
    /// - `turn: Arc<TurnContext>`：单个 turn 的上下文（cwd、审批/沙盒策略、模型、指令等）。
    ///   工具执行应遵从 turn 的约束（比如相对路径解析到 turn.cwd、审批策略、沙盒策略等）。
    /// - `tracker: SharedTurnDiffTracker`：用于追踪本 turn 内文件变更的 diff 追踪器（例如 apply_patch 工具）。
    ///   通过它可以在 turn 结束时汇总 unified diff 发给 UI。
    /// - `call: ToolCall`：模型发起的一次具体工具调用描述：
    ///   - `tool_name`：要调用的工具名；
    ///   - `call_id`：该次调用的唯一标识，工具输出必须携带同一个 id 才能让模型/协议层正确对齐；
    ///   - `payload`：工具参数载荷（Function/Custom/MCP/LocalShell 等），一个payload只有一种参数。
    ///
    /// **返回值解释：**
    /// - 成功时返回 `ResponseInputItem`：这是“写回对话历史/回写给模型”的标准 item，
    ///   可能是 `FunctionCallOutput`、`CustomToolCallOutput` 或 `McpToolCallOutput` 等。
    /// - 失败时返回 `FunctionCallError`：
    ///   - `Fatal`：真正的致命错误，会上抛中断更上层 turn/task；
    ///   - 其它错误：不会上抛，而是被转换成一个“失败的 ResponseInputItem”（`success=false` 或直接输出错误文本），
    ///     让模型在下一轮看到工具失败原因并自行调整策略。
    pub async fn dispatch_tool_call(
        &self,
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        tracker: SharedTurnDiffTracker,
        call: ToolCall,
    ) -> Result<ResponseInputItem, FunctionCallError> {
        let ToolCall {
            tool_name,
            call_id,
            payload,
        } = call;
        // 决定失败时应该回写哪种输出 item：
        // - Custom tool 调用期望 `CustomToolCallOutput`
        // - 普通 function 工具调用期望 `FunctionCallOutput`
        let payload_outputs_custom = matches!(payload, ToolPayload::Custom { .. });
        // call_id 会被移动进 ToolInvocation；这里提前留一份用于构造失败响应。
        let failure_call_id = call_id.clone();

        // 将所有执行所需上下文打包成 ToolInvocation，交给 registry 做真正的“按工具名分发”。
        let invocation = ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
        };

        match self.registry.dispatch(invocation).await {
            Ok(response) => Ok(response),
            // 致命错误：保持语义上抛，交给上层决定如何终止 turn/task。
            Err(FunctionCallError::Fatal(message)) => Err(FunctionCallError::Fatal(message)),
            // 其它错误：转成一个“失败输出”回写给模型（而不是直接中断）。
            Err(err) => Ok(Self::failure_response(
                failure_call_id,
                payload_outputs_custom,
                err,
            )),
        }
    }

    fn failure_response(
        call_id: String,
        payload_outputs_custom: bool,
        err: FunctionCallError,
    ) -> ResponseInputItem {
        let message = err.to_string();
        if payload_outputs_custom {
            ResponseInputItem::CustomToolCallOutput {
                call_id,
                output: message,
            }
        } else {
            ResponseInputItem::FunctionCallOutput {
                call_id,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    content: message,
                    success: Some(false),
                    ..Default::default()
                },
            }
        }
    }
}
