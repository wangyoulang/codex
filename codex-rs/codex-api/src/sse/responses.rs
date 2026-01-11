use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::rate_limits::parse_rate_limit;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use codex_client::TransportError;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures::TryStreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tokio_util::io::ReaderStream;
use tracing::debug;
use tracing::trace;

/// Streams SSE events from an on-disk fixture for tests.
pub fn stream_from_fixture(
    path: impl AsRef<Path>,
    idle_timeout: Duration,
) -> Result<ResponseStream, ApiError> {
    let file =
        std::fs::File::open(path.as_ref()).map_err(|err| ApiError::Stream(err.to_string()))?;
    let mut content = String::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line.map_err(|err| ApiError::Stream(err.to_string()))?;
        content.push_str(&line);
        content.push_str("\n\n");
    }

    let reader = std::io::Cursor::new(content);
    let stream = ReaderStream::new(reader).map_err(|err| TransportError::Network(err.to_string()));
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(process_sse(Box::pin(stream), tx_event, idle_timeout, None));
    Ok(ResponseStream { rx_event })
}

/// 基于一次 StreamResponse 构建 ResponseStream，并启动后台 SSE 解析任务：
/// - 先从响应头提取速率限制与模型 etag，作为事件尽早发给上层；
/// - 再把字节流交给 process_sse，持续解析为 ResponseEvent。
pub fn spawn_response_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) -> ResponseStream {
    // 从响应头解析速率限制快照，优先在流处理前发给上层。
    let rate_limits = parse_rate_limit(&stream_response.headers);
    let models_etag = stream_response
        .headers
        .get("X-Models-Etag")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    // 事件通道：SSE 解析任务往里写，上层从 rx_event 消费。
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(async move {
        // 先推送响应头里的元信息，确保上层尽早拿到环境状态。
        if let Some(snapshot) = rate_limits {
            let _ = tx_event.send(Ok(ResponseEvent::RateLimits(snapshot))).await;
        }
        if let Some(etag) = models_etag {
            let _ = tx_event.send(Ok(ResponseEvent::ModelsEtag(etag))).await;
        }
        // 进入 SSE 主循环：把字节流解析为 ResponseEvent 并发送到通道。
        process_sse(stream_response.bytes, tx_event, idle_timeout, telemetry).await;
    });

    ResponseStream { rx_event }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Error {
    r#type: Option<String>,
    code: Option<String>,
    message: Option<String>,
    plan_type: Option<String>,
    resets_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ResponseCompleted {
    id: String,
    #[serde(default)]
    usage: Option<ResponseCompletedUsage>,
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedUsage {
    input_tokens: i64,
    input_tokens_details: Option<ResponseCompletedInputTokensDetails>,
    output_tokens: i64,
    output_tokens_details: Option<ResponseCompletedOutputTokensDetails>,
    total_tokens: i64,
}

impl From<ResponseCompletedUsage> for TokenUsage {
    fn from(val: ResponseCompletedUsage) -> Self {
        TokenUsage {
            input_tokens: val.input_tokens,
            cached_input_tokens: val
                .input_tokens_details
                .map(|d| d.cached_tokens)
                .unwrap_or(0),
            output_tokens: val.output_tokens,
            reasoning_output_tokens: val
                .output_tokens_details
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0),
            total_tokens: val.total_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedInputTokensDetails {
    cached_tokens: i64,
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedOutputTokensDetails {
    reasoning_tokens: i64,
}

#[derive(Deserialize, Debug)]
struct SseEvent {
    #[serde(rename = "type")]
    kind: String,
    response: Option<Value>,
    item: Option<Value>,
    delta: Option<String>,
    summary_index: Option<i64>,
    content_index: Option<i64>,
}

/// 将底层 HTTP 字节流（SSE：`text/event-stream`）解析为 `ResponseEvent` 并通过 `tx_event` 转发给上层。
///
/// 这个函数是“Responses API → Codex 内部事件流”的核心转换器：
/// - 输入是网络层的 `ByteStream`（来自 reqwest 的 `bytes_stream()` 之类）；
/// - 先用 `eventsource_stream` 把 bytes 解析成一条条 SSE event；
/// - 再把每条 SSE event 的 `data`（JSON）反序列化为 `SseEvent`；
/// - 最后根据 `SseEvent.kind` 映射为项目内部的 `ResponseEvent::*`（OutputItemAdded/Done、各种 delta、Created 等），
///   通过 `tx_event.send(Ok(...))` 送到上层消费。
///
/// 额外行为：
/// - `idle_timeout`：长时间收不到 SSE 时主动报错，避免永远挂起；
/// - `telemetry`：每次 poll SSE 时上报耗时与结果（成功/超时/错误）；
/// - 对 `response.completed` 事件：这里并不立刻向上层发 `ResponseEvent::Completed`，
///   而是先暂存 `response_id/usage`，等到底层 stream 真正结束（收到 `None`）时再补发 Completed；
///   这样可以保证 Completed 是“收尾事件”，避免在连接还会继续推送数据时提前结束。
pub async fn process_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) {
    // 把纯字节流升级为 SSE 事件流：每次 next() 对应读取/解析一条 eventsource 事件。
    let mut stream = stream.eventsource();

    // `response.completed` 的 payload（id/usage）会先存在这里，等到 stream 结束时再统一发 Completed。
    let mut response_completed: Option<ResponseCompleted> = None;

    // `response.failed` 等错误会存在这里；如果 stream 直接断开且没有 completed，就用它作为最终错误。
    let mut response_error: Option<ApiError> = None;

    loop {
        // 每次循环尝试读取一条 SSE event；加上 idle_timeout 防止长连接无响应导致的“假死”。
        let start = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;

        // 记录本次 poll 的结果与耗时：用于统计 SSE 健康度/延迟/超时等。
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }

        // 将 timeout + SSE next() 的返回统一整理成几类：
        // - Ok(Some(Ok(sse)))：拿到一条 SSE event，继续解析；
        // - Ok(Some(Err(e)))：解析 SSE/底层传输错误，直接上报错误并结束；
        // - Ok(None)：流结束；如果见过 response.completed 则补发 Completed，否则上报错误；
        // - Err(_)：idle timeout，直接上报错误并结束。
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("SSE Error: {e:#}");
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => {
                // SSE 流结束：优先补发 Completed；否则返回此前记录的 response_error；
                // 如果也没有记录过错误，则给一个兜底错误信息，表明服务端没正常发 completed 就断开了。
                match response_completed.take() {
                    Some(ResponseCompleted { id, usage }) => {
                        let event = ResponseEvent::Completed {
                            response_id: id,
                            token_usage: usage.map(Into::into),
                        };
                        let _ = tx_event.send(Ok(event)).await;
                    }
                    None => {
                        let error = response_error.unwrap_or(ApiError::Stream(
                            "stream closed before response.completed".into(),
                        ));
                        let _ = tx_event.send(Err(error)).await;
                    }
                }
                return;
            }
            Err(_) => {
                // 等待 SSE 事件超时：把它视为流式错误，交由上层决定是否重试。
                let _ = tx_event
                    .send(Err(ApiError::Stream("idle timeout waiting for SSE".into())))
                    .await;
                return;
            }
        };

        // SSE event 的 data 是 JSON 字符串；这里先留一份原文用于 trace 日志与诊断。
        let raw = sse.data.clone();
        trace!("SSE event: {raw}");

        // 将 SSE JSON 解析为结构化 SseEvent。
        // 解析失败时只记录 debug 并跳过该条（不终止整条流），因为流里可能夹杂不可识别的 event。
        let event: SseEvent = match serde_json::from_str(&sse.data) {
            Ok(event) => event,
            Err(e) => {
                debug!("Failed to parse SSE event: {e}, data: {}", &sse.data);
                continue;
            }
        };

        // 按 OpenAI Responses SSE 的 `type` 字段做事件映射。
        // 映射后的 `ResponseEvent` 会被 core 层（`try_run_turn`）消费，用于：
        // - UI 实时增量渲染（*Delta）；
        // - 落盘对话历史与工具调用（OutputItemAdded/Done）；
        // - 最终收尾（Completed）。
        match event.kind.as_str() {
            "response.output_item.done" => {
                // 一个 output item 已经完整结束（后续不会再有它的 delta）。
                let Some(item_val) = event.item else { continue };
                let Ok(item) = serde_json::from_value::<ResponseItem>(item_val) else {
                    debug!("failed to parse ResponseItem from output_item.done");
                    continue;
                };

                let event = ResponseEvent::OutputItemDone(item);
                if tx_event.send(Ok(event)).await.is_err() {
                    return;
                }
            }
            "response.output_text.delta" => {
                // assistant 文本的增量片段（用于实时追加显示）。
                if let Some(delta) = event.delta {
                    let event = ResponseEvent::OutputTextDelta(delta);
                    if tx_event.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
            "response.reasoning_summary_text.delta" => {
                // 推理摘要（summary）的增量片段：需要 summary_index 指明属于第几段摘要。
                if let (Some(delta), Some(summary_index)) = (event.delta, event.summary_index) {
                    let event = ResponseEvent::ReasoningSummaryDelta {
                        delta,
                        summary_index,
                    };
                    if tx_event.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
            "response.reasoning_text.delta" => {
                // 推理 raw content 的增量片段：需要 content_index 指明属于第几段 raw content。
                if let (Some(delta), Some(content_index)) = (event.delta, event.content_index) {
                    let event = ResponseEvent::ReasoningContentDelta {
                        delta,
                        content_index,
                    };
                    if tx_event.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
            "response.created" => {
                // 响应创建事件：只有当 payload 里带 response 字段时才向上层发 Created。
                if event.response.is_some() {
                    let _ = tx_event.send(Ok(ResponseEvent::Created {})).await;
                }
            }
            "response.failed" => {
                // 响应失败事件：记录为 response_error，供“流断开但没 completed”时作为最终错误返回。
                if let Some(resp_val) = event.response {
                    response_error =
                        Some(ApiError::Stream("response.failed event received".into()));

                    if let Some(error) = resp_val.get("error")
                        && let Ok(error) = serde_json::from_value::<Error>(error.clone())
                    {
                        // 按错误类型细分为：
                        // - ContextWindowExceeded / QuotaExceeded / UsageNotIncluded：明确的不可恢复错误；
                        // - Retryable：包含 message 与可选 delay（例如 rate_limit_exceeded 的 retry-after）。
                        if is_context_window_error(&error) {
                            response_error = Some(ApiError::ContextWindowExceeded);
                        } else if is_quota_exceeded_error(&error) {
                            response_error = Some(ApiError::QuotaExceeded);
                        } else if is_usage_not_included(&error) {
                            response_error = Some(ApiError::UsageNotIncluded);
                        } else {
                            let delay = try_parse_retry_after(&error);
                            let message = error.message.clone().unwrap_or_default();
                            response_error = Some(ApiError::Retryable { message, delay });
                        }
                    }
                }
            }
            "response.completed" => {
                // 注意：这里“只记录 completed”，不立即向上层发 Completed。
                // 真正的 Completed 会在 SSE 流结束（Ok(None)）时补发，确保 Completed 是最终收尾事件。
                if let Some(resp_val) = event.response {
                    match serde_json::from_value::<ResponseCompleted>(resp_val) {
                        Ok(r) => {
                            response_completed = Some(r);
                        }
                        Err(e) => {
                            let error = format!("failed to parse ResponseCompleted: {e}");
                            debug!(error);
                            response_error = Some(ApiError::Stream(error));
                            continue;
                        }
                    };
                };
            }
            "response.output_item.added" => {
                // 一个新的 output item 开始出现（后续可能会有对应的 delta / done）。
                let Some(item_val) = event.item else { continue };
                let Ok(item) = serde_json::from_value::<ResponseItem>(item_val) else {
                    debug!("failed to parse ResponseItem from output_item.done");
                    continue;
                };

                let event = ResponseEvent::OutputItemAdded(item);
                if tx_event.send(Ok(event)).await.is_err() {
                    return;
                }
            }
            "response.reasoning_summary_part.added" => {
                // 推理摘要新增分段：仅携带 summary_index，用于 UI 插入分隔/换段。
                if let Some(summary_index) = event.summary_index {
                    let event = ResponseEvent::ReasoningSummaryPartAdded { summary_index };
                    if tx_event.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
            _ => {}
        }
    }
}

fn try_parse_retry_after(err: &Error) -> Option<Duration> {
    if err.code.as_deref() != Some("rate_limit_exceeded") {
        return None;
    }

    let re = rate_limit_regex();
    if let Some(message) = &err.message
        && let Some(captures) = re.captures(message)
    {
        let seconds = captures.get(1);
        let unit = captures.get(2);

        if let (Some(value), Some(unit)) = (seconds, unit) {
            let value = value.as_str().parse::<f64>().ok()?;
            let unit = unit.as_str().to_ascii_lowercase();

            if unit == "s" || unit.starts_with("second") {
                return Some(Duration::from_secs_f64(value));
            } else if unit == "ms" {
                return Some(Duration::from_millis(value as u64));
            }
        }
    }
    None
}

fn is_context_window_error(error: &Error) -> bool {
    error.code.as_deref() == Some("context_length_exceeded")
}

fn is_quota_exceeded_error(error: &Error) -> bool {
    error.code.as_deref() == Some("insufficient_quota")
}

fn is_usage_not_included(error: &Error) -> bool {
    error.code.as_deref() == Some("usage_not_included")
}

fn rate_limit_regex() -> &'static regex_lite::Regex {
    static RE: std::sync::OnceLock<regex_lite::Regex> = std::sync::OnceLock::new();
    #[expect(clippy::unwrap_used)]
    RE.get_or_init(|| {
        regex_lite::Regex::new(r"(?i)try again in\s*(\d+(?:\.\d+)?)\s*(s|ms|seconds?)").unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use codex_protocol::models::ResponseItem;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_test::io::Builder as IoBuilder;

    async fn collect_events(chunks: &[&[u8]]) -> Vec<Result<ResponseEvent, ApiError>> {
        let mut builder = IoBuilder::new();
        for chunk in chunks {
            builder.read(chunk);
        }

        let reader = builder.build();
        let stream =
            ReaderStream::new(reader).map_err(|err| TransportError::Network(err.to_string()));
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        tokio::spawn(process_sse(Box::pin(stream), tx, idle_timeout(), None));

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        events
    }

    async fn run_sse(events: Vec<serde_json::Value>) -> Vec<ResponseEvent> {
        let mut body = String::new();
        for e in events {
            let kind = e
                .get("type")
                .and_then(|v| v.as_str())
                .expect("fixture event missing type");
            if e.as_object().map(|o| o.len() == 1).unwrap_or(false) {
                body.push_str(&format!("event: {kind}\n\n"));
            } else {
                body.push_str(&format!("event: {kind}\ndata: {e}\n\n"));
            }
        }

        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(8);
        let stream = ReaderStream::new(std::io::Cursor::new(body))
            .map_err(|err| TransportError::Network(err.to_string()));
        tokio::spawn(process_sse(Box::pin(stream), tx, idle_timeout(), None));

        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev.expect("channel closed"));
        }
        out
    }

    fn idle_timeout() -> Duration {
        Duration::from_millis(1000)
    }

    #[tokio::test]
    async fn parses_items_and_completed() {
        let item1 = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Hello"}]
            }
        })
        .to_string();

        let item2 = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "World"}]
            }
        })
        .to_string();

        let completed = json!({
            "type": "response.completed",
            "response": { "id": "resp1" }
        })
        .to_string();

        let sse1 = format!("event: response.output_item.done\ndata: {item1}\n\n");
        let sse2 = format!("event: response.output_item.done\ndata: {item2}\n\n");
        let sse3 = format!("event: response.completed\ndata: {completed}\n\n");

        let events = collect_events(&[sse1.as_bytes(), sse2.as_bytes(), sse3.as_bytes()]).await;

        assert_eq!(events.len(), 3);

        assert_matches!(
            &events[0],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { role, .. }))
                if role == "assistant"
        );

        assert_matches!(
            &events[1],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { role, .. }))
                if role == "assistant"
        );

        match &events[2] {
            Ok(ResponseEvent::Completed {
                response_id,
                token_usage,
            }) => {
                assert_eq!(response_id, "resp1");
                assert!(token_usage.is_none());
            }
            other => panic!("unexpected third event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_when_missing_completed() {
        let item1 = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Hello"}]
            }
        })
        .to_string();

        let sse1 = format!("event: response.output_item.done\ndata: {item1}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 2);

        assert_matches!(events[0], Ok(ResponseEvent::OutputItemDone(_)));

        match &events[1] {
            Err(ApiError::Stream(msg)) => {
                assert_eq!(msg, "stream closed before response.completed")
            }
            other => panic!("unexpected second event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_when_error_event() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_689bcf18d7f08194bf3440ba62fe05d803fee0cdac429894","object":"response","created_at":1755041560,"status":"failed","background":false,"error":{"code":"rate_limit_exceeded","message":"Rate limit reached for gpt-5.1 in organization org-AAA on tokens per min (TPM): Limit 30000, Used 22999, Requested 12528. Please try again in 11.054s. Visit https://platform.openai.com/account/rate-limits to learn more."}, "usage":null,"user":null,"metadata":{}}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 1);

        match &events[0] {
            Err(ApiError::Retryable { message, delay }) => {
                assert_eq!(
                    message,
                    "Rate limit reached for gpt-5.1 in organization org-AAA on tokens per min (TPM): Limit 30000, Used 22999, Requested 12528. Please try again in 11.054s. Visit https://platform.openai.com/account/rate-limits to learn more."
                );
                assert_eq!(*delay, Some(Duration::from_secs_f64(11.054)));
            }
            other => panic!("unexpected second event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_window_error_is_fatal() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_5c66275b97b9baef1ed95550adb3b7ec13b17aafd1d2f11b","object":"response","created_at":1759510079,"status":"failed","background":false,"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again."},"usage":null,"user":null,"metadata":{}}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 1);

        assert_matches!(events[0], Err(ApiError::ContextWindowExceeded));
    }

    #[tokio::test]
    async fn context_window_error_with_newline_is_fatal() {
        let raw_error = r#"{"type":"response.failed","sequence_number":4,"response":{"id":"resp_fatal_newline","object":"response","created_at":1759510080,"status":"failed","background":false,"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try\nagain."},"usage":null,"user":null,"metadata":{}}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 1);

        assert_matches!(events[0], Err(ApiError::ContextWindowExceeded));
    }

    #[tokio::test]
    async fn quota_exceeded_error_is_fatal() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_fatal_quota","object":"response","created_at":1759771626,"status":"failed","background":false,"error":{"code":"insufficient_quota","message":"You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: https://platform.openai.com/docs/guides/error-codes/api-errors."},"incomplete_details":null}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 1);

        assert_matches!(events[0], Err(ApiError::QuotaExceeded));
    }

    #[tokio::test]
    async fn table_driven_event_kinds() {
        struct TestCase {
            name: &'static str,
            event: serde_json::Value,
            expect_first: fn(&ResponseEvent) -> bool,
            expected_len: usize,
        }

        fn is_created(ev: &ResponseEvent) -> bool {
            matches!(ev, ResponseEvent::Created)
        }
        fn is_output(ev: &ResponseEvent) -> bool {
            matches!(ev, ResponseEvent::OutputItemDone(_))
        }
        fn is_completed(ev: &ResponseEvent) -> bool {
            matches!(ev, ResponseEvent::Completed { .. })
        }

        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": "c",
                "usage": {
                    "input_tokens": 0,
                    "input_tokens_details": null,
                    "output_tokens": 0,
                    "output_tokens_details": null,
                    "total_tokens": 0
                },
                "output": []
            }
        });

        let cases = vec![
            TestCase {
                name: "created",
                event: json!({"type": "response.created", "response": {}}),
                expect_first: is_created,
                expected_len: 2,
            },
            TestCase {
                name: "output_item.done",
                event: json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {"type": "output_text", "text": "hi"}
                        ]
                    }
                }),
                expect_first: is_output,
                expected_len: 2,
            },
            TestCase {
                name: "unknown",
                event: json!({"type": "response.new_tool_event"}),
                expect_first: is_completed,
                expected_len: 1,
            },
        ];

        for case in cases {
            let mut evs = vec![case.event];
            evs.push(completed.clone());

            let out = run_sse(evs).await;
            assert_eq!(out.len(), case.expected_len, "case {}", case.name);
            assert!(
                (case.expect_first)(&out[0]),
                "first event mismatch in case {}",
                case.name
            );
        }
    }

    #[test]
    fn test_try_parse_retry_after() {
        let err = Error {
            r#type: None,
            message: Some("Rate limit reached for gpt-5.1 in organization org- on tokens per min (TPM): Limit 1, Used 1, Requested 19304. Please try again in 28ms. Visit https://platform.openai.com/account/rate-limits to learn more.".to_string()),
            code: Some("rate_limit_exceeded".to_string()),
            plan_type: None,
            resets_at: None,
        };

        let delay = try_parse_retry_after(&err);
        assert_eq!(delay, Some(Duration::from_millis(28)));
    }

    #[test]
    fn test_try_parse_retry_after_no_delay() {
        let err = Error {
            r#type: None,
            message: Some("Rate limit reached for gpt-5.1 in organization <ORG> on tokens per min (TPM): Limit 30000, Used 6899, Requested 24050. Please try again in 1.898s. Visit https://platform.openai.com/account/rate-limits to learn more.".to_string()),
            code: Some("rate_limit_exceeded".to_string()),
            plan_type: None,
            resets_at: None,
        };
        let delay = try_parse_retry_after(&err);
        assert_eq!(delay, Some(Duration::from_secs_f64(1.898)));
    }

    #[test]
    fn test_try_parse_retry_after_azure() {
        let err = Error {
            r#type: None,
            message: Some("Rate limit exceeded. Try again in 35 seconds.".to_string()),
            code: Some("rate_limit_exceeded".to_string()),
            plan_type: None,
            resets_at: None,
        };
        let delay = try_parse_retry_after(&err);
        assert_eq!(delay, Some(Duration::from_secs(35)));
    }
}
