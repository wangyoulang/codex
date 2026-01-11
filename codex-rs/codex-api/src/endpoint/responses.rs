use crate::auth::AuthProvider;
use crate::common::Prompt as ApiPrompt;
use crate::common::Reasoning;
use crate::common::ResponseStream;
use crate::common::TextControls;
use crate::endpoint::streaming::StreamingClient;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::provider::WireApi;
use crate::requests::ResponsesRequest;
use crate::requests::ResponsesRequestBuilder;
use crate::sse::spawn_response_stream;
use crate::telemetry::SseTelemetry;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use serde_json::Value;
use std::sync::Arc;
use tracing::instrument;

pub struct ResponsesClient<T: HttpTransport, A: AuthProvider> {
    streaming: StreamingClient<T, A>,
}

#[derive(Default)]
pub struct ResponsesOptions {
    pub reasoning: Option<Reasoning>,
    pub include: Vec<String>,
    pub prompt_cache_key: Option<String>,
    pub text: Option<TextControls>,
    pub store_override: Option<bool>,
    pub conversation_id: Option<String>,
    pub session_source: Option<SessionSource>,
    pub extra_headers: HeaderMap,
}

impl<T: HttpTransport, A: AuthProvider> ResponsesClient<T, A> {
    pub fn new(transport: T, provider: Provider, auth: A) -> Self {
        Self {
            streaming: StreamingClient::new(transport, provider, auth),
        }
    }

    pub fn with_telemetry(
        self,
        request: Option<Arc<dyn RequestTelemetry>>,
        sse: Option<Arc<dyn SseTelemetry>>,
    ) -> Self {
        Self {
            streaming: self.streaming.with_telemetry(request, sse),
        }
    }

    pub async fn stream_request(
        &self,
        request: ResponsesRequest,
    ) -> Result<ResponseStream, ApiError> {
        self.stream(request.body, request.headers).await
    }

    /// 把通用的 `Prompt + ResponsesOptions` 组装为 Responses API 请求并发起流式 SSE。
    ///
    /// 主要职责：
    /// - 将工具声明、并行工具调用开关、reasoning/verbosity/text 控制、缓存 key、会话来源等元数据
    ///   写入 `ResponsesRequest`；
    /// - 按 provider 的 wire 类型选择正确的路径/头部（通过 builder + `stream_request` 完成）；
    /// - 返回标准化的 `ResponseStream`，供上层统一消费。
    #[instrument(level = "trace", skip_all, err)]
    pub async fn stream_prompt(
        &self,
        model: &str,
        prompt: &ApiPrompt,
        options: ResponsesOptions,
    ) -> Result<ResponseStream, ApiError> {
        let ResponsesOptions {
            reasoning,
            include,
            prompt_cache_key,
            text,
            store_override,
            conversation_id,
            session_source,
            extra_headers,
        } = options;

        let request = ResponsesRequestBuilder::new(model, &prompt.instructions, &prompt.input)
            .tools(&prompt.tools)
            .parallel_tool_calls(prompt.parallel_tool_calls)
            .reasoning(reasoning)
            .include(include)
            .prompt_cache_key(prompt_cache_key)
            .text(text)
            .conversation(conversation_id)
            .session_source(session_source)
            .store_override(store_override)
            .extra_headers(extra_headers)
            .build(self.streaming.provider())?;

        self.stream_request(request).await
    }

    fn path(&self) -> &'static str {
        match self.streaming.provider().wire {
            WireApi::Responses | WireApi::Compact => "responses",
            WireApi::Chat => "chat/completions",
        }
    }

    pub async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        // 选择当前 provider 对应的路径（responses / chat/completions），
        // 并把请求体与额外头部交给底层 StreamingClient 发送。
        // `spawn_response_stream` 负责把底层 HTTP 长连接包装成项目内部的 ResponseStream。
        self.streaming
            .stream(self.path(), body, extra_headers, spawn_response_stream)
            .await
    }
}
