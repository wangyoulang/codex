use http::Error as HttpError;
use opentelemetry::global;
use opentelemetry::propagation::Injector;
use reqwest::IntoUrl;
use reqwest::Method;
use reqwest::Response;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Display;
use std::time::Duration;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

#[derive(Clone, Debug)]
pub struct CodexHttpClient {
    inner: reqwest::Client,
}

impl CodexHttpClient {
    pub fn new(inner: reqwest::Client) -> Self {
        Self { inner }
    }

    pub fn get<U>(&self, url: U) -> CodexRequestBuilder
    where
        U: IntoUrl,
    {
        self.request(Method::GET, url)
    }

    pub fn post<U>(&self, url: U) -> CodexRequestBuilder
    where
        U: IntoUrl,
    {
        self.request(Method::POST, url)
    }

    pub fn request<U>(&self, method: Method, url: U) -> CodexRequestBuilder
    where
        U: IntoUrl,
    {
        let url_str = url.as_str().to_string();
        CodexRequestBuilder::new(self.inner.request(method.clone(), url), method, url_str)
    }
}

#[must_use = "requests are not sent unless `send` is awaited"]
#[derive(Debug)]
pub struct CodexRequestBuilder {
    builder: reqwest::RequestBuilder,
    method: Method,
    url: String,
}

impl CodexRequestBuilder {
    fn new(builder: reqwest::RequestBuilder, method: Method, url: String) -> Self {
        Self {
            builder,
            method,
            url,
        }
    }

    fn map(self, f: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> Self {
        Self {
            builder: f(self.builder),
            method: self.method,
            url: self.url,
        }
    }

    pub fn headers(self, headers: HeaderMap) -> Self {
        self.map(|builder| builder.headers(headers))
    }

    pub fn header<K, V>(self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        self.map(|builder| builder.header(key, value))
    }

    pub fn bearer_auth<T>(self, token: T) -> Self
    where
        T: Display,
    {
        self.map(|builder| builder.bearer_auth(token))
    }

    pub fn timeout(self, timeout: Duration) -> Self {
        self.map(|builder| builder.timeout(timeout))
    }

    pub fn json<T>(self, value: &T) -> Self
    where
        T: ?Sized + Serialize,
    {
        self.map(|builder| builder.json(value))
    }

    /// 发送 HTTP 请求并返回 reqwest 的 `Response`。
    ///
    /// 这个封装的核心目的不是“改造请求语义”，而是为了把 Codex 关心的诊断信息统一打到日志里：
    /// - 追加 `trace_headers()` 返回的调试/链路追踪相关 header（如果启用）；
    /// - 在成功时打印 method/url/status/http version，并尽可能从响应头中提取 request id；
    /// - 在失败时打印 method/url/可选 status 以及错误本身，便于定位网络/鉴权/服务端错误。
    pub async fn send(self) -> Result<Response, reqwest::Error> {
        // 生成需要附加到本次请求的“追踪/调试”头（例如 request id、trace 相关字段等）。
        // 这些头只影响可观测性，不应改变业务语义。
        let headers = trace_headers();

        // 在真正发送前把 headers 合并到 builder 上。
        match self.builder.headers(headers).send().await {
            Ok(response) => {
                // 成功响应：从响应头里提取一组常见的 request id（不同网关/服务会用不同 header 命名）。
                let request_ids = Self::extract_request_ids(&response);
                tracing::debug!(
                    method = %self.method,
                    url = %self.url,
                    status = %response.status(),
                    request_ids = ?request_ids,
                    version = ?response.version(),
                    "Request completed"
                );

                Ok(response)
            }
            Err(error) => {
                // 失败响应：reqwest 的 Error 可能带 status（例如非 2xx），也可能没有（网络错误/超时等）。
                let status = error.status();
                tracing::debug!(
                    method = %self.method,
                    url = %self.url,
                    status = status.map(|s| s.as_u16()),
                    error = %error,
                    "Request failed"
                );
                Err(error)
            }
        }
    }

    fn extract_request_ids(response: &Response) -> HashMap<String, String> {
        ["cf-ray", "x-request-id", "x-oai-request-id"]
            .iter()
            .filter_map(|&name| {
                let header_name = HeaderName::from_static(name);
                let value = response.headers().get(header_name)?;
                let value = value.to_str().ok()?.to_owned();
                Some((name.to_owned(), value))
            })
            .collect()
    }
}

struct HeaderMapInjector<'a>(&'a mut HeaderMap);

impl<'a> Injector for HeaderMapInjector<'a> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, val);
        }
    }
}

fn trace_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    global::get_text_map_propagator(|prop| {
        prop.inject_context(
            &Span::current().context(),
            &mut HeaderMapInjector(&mut headers),
        );
    });
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::propagation::Extractor;
    use opentelemetry::propagation::TextMapPropagator;
    use opentelemetry::trace::TraceContextExt;
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing::trace_span;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    #[test]
    fn inject_trace_headers_uses_current_span_context() {
        global::set_text_map_propagator(TraceContextPropagator::new());

        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("test-tracer");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        let _guard = subscriber.set_default();

        let span = trace_span!("client_request");
        let _entered = span.enter();
        let span_context = span.context().span().span_context().clone();

        let headers = trace_headers();

        let extractor = HeaderMapExtractor(&headers);
        let extracted = TraceContextPropagator::new().extract(&extractor);
        let extracted_span = extracted.span();
        let extracted_context = extracted_span.span_context();

        assert!(extracted_context.is_valid());
        assert_eq!(extracted_context.trace_id(), span_context.trace_id());
        assert_eq!(extracted_context.span_id(), span_context.span_id());
    }

    struct HeaderMapExtractor<'a>(&'a HeaderMap);

    impl<'a> Extractor for HeaderMapExtractor<'a> {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).and_then(|value| value.to_str().ok())
        }

        fn keys(&self) -> Vec<&str> {
            self.0.keys().map(HeaderName::as_str).collect()
        }
    }
}
