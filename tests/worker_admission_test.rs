mod common;

use axum::http::StatusCode;
use common::mock_worker::{HealthStatus, MockWorker, MockWorkerConfig, WorkerType};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use vllm_router_rs::config::{PolicyConfig, RouterConfig, RoutingMode};
use vllm_router_rs::core::WorkerAdmissionConfig;
use vllm_router_rs::protocols::spec::ChatCompletionRequest;
use vllm_router_rs::routers::{RouterFactory, RouterTrait};

fn chat_request() -> ChatCompletionRequest {
    serde_json::from_value(json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": false,
        "max_tokens": 10
    }))
    .unwrap()
}

async fn transparent_chat_call(
    router: Arc<dyn RouterTrait>,
    body: serde_json::Value,
) -> StatusCode {
    let response = router
        .route_transparent(None, "/v1/responses", &axum::http::Method::POST, body)
        .await;
    let status = response.status();
    // Consume the response body so transparent-path stream permits are released.
    axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    status
}

#[tokio::test]
async fn per_worker_gate_queues_excess_and_rejects_when_full() {
    let mut worker = MockWorker::new(MockWorkerConfig {
        port: 20701,
        worker_type: WorkerType::Regular,
        health_status: HealthStatus::Healthy,
        response_delay_ms: 800,
        fail_rate: 0.0,
    });
    let url = worker.start().await.unwrap();

    let config = RouterConfig {
        mode: RoutingMode::Regular {
            worker_urls: vec![url],
        },
        policy: PolicyConfig::Random,
        worker_startup_timeout_secs: 1,
        worker_startup_check_interval_secs: 1,
        request_timeout_secs: 10,
        ..Default::default()
    };

    let admission = WorkerAdmissionConfig {
        max_concurrent_requests_per_worker: Some(1),
        worker_queue_size: 1,
        queue_timeout: Duration::from_secs(5),
    };
    let app_context = common::create_test_context_with_admission(config, admission);
    let router: Arc<dyn RouterTrait> =
        Arc::from(RouterFactory::create_router(&app_context).await.unwrap());

    let body = chat_request();
    let (r1, r2, r3) = tokio::join!(
        router.route_chat(None, &body, None),
        router.route_chat(None, &body, None),
        router.route_chat(None, &body, None),
    );

    let statuses = vec![r1.status(), r2.status(), r3.status()];

    // One request occupies the single worker slot, one waits in the queue,
    // and one is rejected because the per-worker queue is full.
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
            .count(),
        1
    );
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::OK).count(), 2);

    worker.stop().await;
}

#[tokio::test]
async fn per_worker_gate_also_covers_transparent_proxy() {
    let mut worker = MockWorker::new(MockWorkerConfig {
        port: 20702,
        worker_type: WorkerType::Regular,
        health_status: HealthStatus::Healthy,
        response_delay_ms: 800,
        fail_rate: 0.0,
    });
    let url = worker.start().await.unwrap();

    let config = RouterConfig {
        mode: RoutingMode::Regular {
            worker_urls: vec![url],
        },
        policy: PolicyConfig::Random,
        worker_startup_timeout_secs: 1,
        worker_startup_check_interval_secs: 1,
        request_timeout_secs: 10,
        ..Default::default()
    };
    let admission = WorkerAdmissionConfig {
        max_concurrent_requests_per_worker: Some(1),
        worker_queue_size: 1,
        queue_timeout: Duration::from_secs(5),
    };
    let app_context = common::create_test_context_with_admission(config, admission);
    let router: Arc<dyn RouterTrait> =
        Arc::from(RouterFactory::create_router(&app_context).await.unwrap());

    let body = json!({
        "model": "test-model",
        "input": "hello",
        "stream": false
    });
    let (s1, s2, s3) = tokio::join!(
        transparent_chat_call(Arc::clone(&router), body.clone()),
        transparent_chat_call(Arc::clone(&router), body.clone()),
        transparent_chat_call(Arc::clone(&router), body),
    );

    let statuses = vec![s1, s2, s3];
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
            .count(),
        1
    );
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::OK).count(), 2);

    worker.stop().await;
}
