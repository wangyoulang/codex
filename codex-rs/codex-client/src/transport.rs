use crate::default_client::CodexHttpClient;
use crate::default_client::CodexRequestBuilder;
use crate::error::TransportError;
use crate::request::Request;
use crate::request::Response;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use http::HeaderMap;
use http::Method;
use http::StatusCode;
use tracing::Level;
use tracing::enabled;
use tracing::trace;

pub type ByteStream = BoxStream<'static, Result<Bytes, TransportError>>;

pub struct StreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: ByteStream,
}

#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// 一次性请求：发送 HTTP 请求并返回完整响应体（已读取完的 bytes）。
    async fn execute(&self, req: Request) -> Result<Response, TransportError>;
    /// 流式请求：发送 HTTP 请求并返回一个可逐步消费的字节流（用于 SSE 等长连接）。
    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError>;
}

#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: CodexHttpClient,
}

impl ReqwestTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client: CodexHttpClient::new(client),
        }
    }

    fn build(&self, req: Request) -> Result<CodexRequestBuilder, TransportError> {
        let mut builder = self
            .client
            .request(
                Method::from_bytes(req.method.as_str().as_bytes()).unwrap_or(Method::GET),
                &req.url,
            )
            .headers(req.headers);
        if let Some(timeout) = req.timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(body) = req.body {
            builder = builder.json(&body);
        }
        Ok(builder)
    }

    fn map_error(err: reqwest::Error) -> TransportError {
        if err.is_timeout() {
            TransportError::Timeout
        } else {
            TransportError::Network(err.to_string())
        }
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(&self, req: Request) -> Result<Response, TransportError> {
        let builder = self.build(req)?;
        let resp = builder.send().await.map_err(Self::map_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await.map_err(Self::map_error)?;
        if !status.is_success() {
            let body = String::from_utf8(bytes.to_vec()).ok();
            return Err(TransportError::Http {
                status,
                headers: Some(headers),
                body,
            });
        }
        Ok(Response {
            status,
            headers,
            body: bytes,
        })
    }

    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError> {
        // `stream` 用于“流式响应”的场景（典型是 SSE：text/event-stream）。
        // 与 `execute` 的区别是：这里不会一次性把响应体读完，而是把底层网络连接上的 bytes 以 stream 形式向上层暴露，
        // 由上层协议解析器（例如 SSE 解析器）按需逐块读取并解析事件。

        if enabled!(Level::TRACE) {
            // trace 级别下打印本次请求的关键信息，便于排查：
            // - method/url
            // - body（如果为空则打印默认值）
            trace!(
                "{} to {}: {}",
                req.method,
                req.url,
                req.body.as_ref().unwrap_or_default()
            );
        }

        // 将项目内部的 `Request` 转换为 reqwest 的请求 builder：
        // - method/url/headers
        // - timeout（如果设置）
        // - body（如果设置，会按 json 发送）
        let builder = self.build(req)?;

        // 真正发起网络请求：这里会建立 HTTP 连接并拿到响应头，随后可以持续读取响应体字节流。
        let resp = builder.send().await.map_err(Self::map_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        if !status.is_success() {
            // 非 2xx：把响应体（如果能读到）作为字符串返回，方便上层打印错误详情。
            // 注意：这里读 `resp.text()` 会消费响应体；在错误分支我们不再需要流式读取。
            let body = resp.text().await.ok();
            return Err(TransportError::Http {
                status,
                headers: Some(headers),
                body,
            });
        }

        // 成功响应：把 reqwest 的 bytes_stream 转成项目内部 `ByteStream`。
        // 这里不做任何协议层解析（例如 SSE/eventsource），只负责把网络层的字节流往上递交。
        let stream = resp
            .bytes_stream()
            .map(|result| result.map_err(Self::map_error));

        // 返回 `StreamResponse`：
        // - status/headers：上层可能用于诊断或读取 rate limit 等信息
        // - bytes：可被持续 poll 的字节流
        Ok(StreamResponse {
            status,
            headers,
            bytes: Box::pin(stream),
        })
    }
}
