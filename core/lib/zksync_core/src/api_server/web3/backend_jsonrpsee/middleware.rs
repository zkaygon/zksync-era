use std::{
    collections::HashSet,
    future::Future,
    num::NonZeroU32,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use governor::{
    clock::DefaultClock,
    middleware::NoOpMiddleware,
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter,
};
use pin_project_lite::pin_project;
use vise::{
    Buckets, Counter, EncodeLabelSet, EncodeLabelValue, Family, GaugeGuard, Histogram, Metrics,
};
use zksync_web3_decl::jsonrpsee::{
    server::middleware::rpc::{layer::ResponseFuture, RpcServiceT},
    types::{error::ErrorCode, ErrorObject, Request},
    MethodResponse,
};

use super::metadata::{MethodCall, MethodTracer};
use crate::api_server::web3::metrics::API_METRICS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue, EncodeLabelSet)]
#[metrics(label = "transport", rename_all = "snake_case")]
pub(crate) enum Transport {
    Ws,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "api_jsonrpc_backend_batch")]
struct LimitMiddlewareMetrics {
    /// Number of rate-limited requests.
    rate_limited: Family<Transport, Counter>,
    /// Size of batch requests.
    #[metrics(buckets = Buckets::exponential(1.0..=512.0, 2.0))]
    size: Family<Transport, Histogram<usize>>,
    /// Number of requests rejected by the limiter.
    rejected: Family<Transport, Counter>,
}

#[vise::register]
static METRICS: vise::Global<LimitMiddlewareMetrics> = vise::Global::new();

/// A rate-limiting middleware.
///
/// `jsonrpsee` will allocate the instance of this struct once per session.
pub(crate) struct LimitMiddleware<S> {
    inner: S,
    rate_limiter: Option<RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>>,
    transport: Transport,
    _guard: GaugeGuard,
}

impl<S> LimitMiddleware<S> {
    pub(crate) fn new(inner: S, requests_per_minute_limit: Option<NonZeroU32>) -> Self {
        Self {
            inner,
            rate_limiter: requests_per_minute_limit
                .map(|limit| RateLimiter::direct(Quota::per_minute(limit))),
            transport: Transport::Ws,
            _guard: API_METRICS.ws_open_sessions.inc_guard(1),
        }
    }
}

impl<'a, S> RpcServiceT<'a> for LimitMiddleware<S>
where
    S: Send + Sync + RpcServiceT<'a>,
{
    type Future = ResponseFuture<S::Future>;

    fn call(&self, request: Request<'a>) -> Self::Future {
        if let Some(rate_limiter) = &self.rate_limiter {
            let num_requests = NonZeroU32::MIN; // 1 request, no batches possible

            // Note: if required, we can extract data on rate limiting from the error.
            if rate_limiter.check_n(num_requests).is_err() {
                METRICS.rate_limited[&self.transport].inc();

                let rp = MethodResponse::error(
                    request.id,
                    ErrorObject::borrowed(
                        ErrorCode::ServerError(
                            reqwest::StatusCode::TOO_MANY_REQUESTS.as_u16().into(),
                        )
                        .code(),
                        "Too many requests",
                        None,
                    ),
                );
                return ResponseFuture::ready(rp);
            }
        }
        ResponseFuture::future(self.inner.call(request))
    }
}

#[derive(Debug)]
pub(crate) struct MetadataMiddleware<S> {
    inner: S,
    registered_method_names: Arc<HashSet<&'static str>>,
    method_tracer: Arc<MethodTracer>,
}

impl<S> MetadataMiddleware<S> {
    pub fn new(
        inner: S,
        registered_method_names: Arc<HashSet<&'static str>>,
        method_tracer: Arc<MethodTracer>,
    ) -> Self {
        Self {
            inner,
            registered_method_names,
            method_tracer,
        }
    }
}

impl<'a, S> RpcServiceT<'a> for MetadataMiddleware<S>
where
    S: Send + Sync + RpcServiceT<'a>,
{
    type Future = WithMethodCall<S::Future>;

    fn call(&self, request: Request<'a>) -> Self::Future {
        // "Normalize" the method name by searching it in the set of all registered methods. This extends the lifetime
        // of the name to `'static` and maps unknown methods to "", so that method name metric labels don't have unlimited cardinality.
        let method_name = self
            .registered_method_names
            .get(request.method_name())
            .copied()
            .unwrap_or("");

        WithMethodCall {
            call: self.method_tracer.new_call(method_name),
            inner: self.inner.call(request),
        }
    }
}

pin_project! {
    #[derive(Debug)]
    pub(crate) struct WithMethodCall<F> {
        call: MethodCall,
        #[pin]
        inner: F,
    }
}

impl<F: Future<Output = MethodResponse>> Future for WithMethodCall<F> {
    type Output = MethodResponse;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let projection = self.project();
        let guard = projection.call.set_as_current();
        match projection.inner.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(response) => {
                drop(guard);
                projection.call.observe_response(&response);
                Poll::Ready(response)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use jsonrpsee::helpers::MethodResponseResult;
    use rand::{thread_rng, Rng};
    use test_casing::{test_casing, Product};
    use zksync_types::api;

    use super::*;

    #[test_casing(4, Product(([false, true], [false, true])))]
    #[tokio::test(flavor = "multi_thread")]
    async fn metadata_middleware_basics(spawn_tasks: bool, sleep: bool) {
        let method_tracer = Arc::new(MethodTracer::default());

        let tasks = (0_u64..100).map(|i| {
            let current_method = method_tracer.clone();
            let inner = async move {
                assert_eq!(current_method.meta().unwrap().block_id, None);
                current_method.set_block_id(api::BlockId::Number(i.into()));

                for diff in 0_u32..10 {
                    let meta = current_method.meta().unwrap();
                    assert_eq!(meta.block_id, Some(api::BlockId::Number(i.into())));
                    assert_eq!(meta.block_diff, diff.checked_sub(1));
                    current_method.set_block_diff(diff);

                    if sleep {
                        let delay = thread_rng().gen_range(1..=5);
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    } else {
                        tokio::task::yield_now().await;
                    }
                }

                MethodResponse {
                    result: "{}".to_string(),
                    success_or_error: MethodResponseResult::Success,
                    is_subscription: false,
                }
            };

            WithMethodCall {
                call: method_tracer.new_call("test"),
                inner,
            }
        });

        if spawn_tasks {
            let tasks: Vec<_> = tasks.map(tokio::spawn).collect();
            for task in tasks {
                task.await.unwrap();
            }
        } else {
            futures::future::join_all(tasks).await;
        }

        let calls = method_tracer.recorded_calls().take();
        assert_eq!(calls.len(), 100);
        for call in &calls {
            assert_eq!(call.metadata.name, "test");
            assert!(call.metadata.block_id.is_some());
            assert_eq!(call.metadata.block_diff, Some(9));
            assert!(call.response.is_success());
        }
    }
}
