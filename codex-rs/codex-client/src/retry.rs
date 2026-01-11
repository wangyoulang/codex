use crate::error::TransportError;
use crate::request::Request;
use rand::Rng;
use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u64,
    pub base_delay: Duration,
    pub retry_on: RetryOn,
}

#[derive(Debug, Clone)]
pub struct RetryOn {
    pub retry_429: bool,
    pub retry_5xx: bool,
    pub retry_transport: bool,
}

impl RetryOn {
    pub fn should_retry(&self, err: &TransportError, attempt: u64, max_attempts: u64) -> bool {
        if attempt >= max_attempts {
            return false;
        }
        match err {
            TransportError::Http { status, .. } => {
                (self.retry_429 && status.as_u16() == 429)
                    || (self.retry_5xx && status.is_server_error())
            }
            TransportError::Timeout | TransportError::Network(_) => self.retry_transport,
            _ => false,
        }
    }
}

pub fn backoff(base: Duration, attempt: u64) -> Duration {
    if attempt == 0 {
        return base;
    }
    let exp = 2u64.saturating_pow(attempt as u32 - 1);
    let millis = base.as_millis() as u64;
    let raw = millis.saturating_mul(exp);
    let jitter: f64 = rand::rng().random_range(0.9..1.1);
    Duration::from_millis((raw as f64 * jitter) as u64)
}

/// 带重试的通用执行器。
///
/// 参数说明：
/// - `policy`：重试策略，包含最大尝试次数、基础退避时间、可重试错误类别；
/// - `make_req`：用于每次尝试重新构建 `Request` 的闭包（可携带最新的鉴权/头部等）；
/// - `op`：实际发送请求的异步闭包，签名为 `(Request, attempt) -> Future<Output = Result<T, TransportError>>`。
///   注意调用方看起来会“传入三个参数”：`run_with_retry(policy, make_req, op)`；
///   第四个 `attempt` 由 `run_with_retry` 在循环中作为第二个参数传给 `op`，因此函数本身只需要这三个入参。
pub async fn run_with_retry<T, F, Fut>(
    policy: RetryPolicy,
    mut make_req: impl FnMut() -> Request,
    op: F,
) -> Result<T, TransportError>
where
    F: Fn(Request, u64) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    for attempt in 0..=policy.max_attempts {
        let req = make_req();
        match op(req, attempt).await {
            Ok(resp) => return Ok(resp),
            Err(err)
                if policy
                    .retry_on
                    .should_retry(&err, attempt, policy.max_attempts) =>
            {
                sleep(backoff(policy.base_delay, attempt + 1)).await;
            }
            Err(err) => return Err(err),
        }
    }
    Err(TransportError::RetryLimit)
}
