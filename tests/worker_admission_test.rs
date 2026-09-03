mod common;

use axum::http::StatusCode;
use common::mock_worker::{HealthStatus, MockWorker, MockWorkerConfig, WorkerType};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use vllm_router_rs::config::{PolicyConfig, RouterConfig, RoutingMode, SessionAffinityConfig};
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

fn url_port(url: &str) -> u16 {
    url.rsplit(':').next().unwrap().parse().unwrap()
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
async fn zero_queue_timeout_keeps_waiting_until_slot_is_free() {
    let mut worker = MockWorker::new(MockWorkerConfig {
        port: 20703,
        worker_type: WorkerType::Regular,
        health_status: HealthStatus::Healthy,
        response_delay_ms: 300,
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

    // queue_timeout == 0 means the second request must wait (not fail with
    // 408) until the first request releases the only per-worker slot.
    let admission = WorkerAdmissionConfig {
        max_concurrent_requests_per_worker: Some(1),
        worker_queue_size: 1,
        queue_timeout: Duration::ZERO,
    };
    let app_context = common::create_test_context_with_admission(config, admission);
    let router: Arc<dyn RouterTrait> =
        Arc::from(RouterFactory::create_router(&app_context).await.unwrap());

    let body = chat_request();
    let (r1, r2) = tokio::join!(
        router.route_chat(None, &body, None),
        router.route_chat(None, &body, None),
    );

    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(r2.status(), StatusCode::OK);

    worker.stop().await;
}

#[tokio::test]
async fn queued_request_fails_over_when_worker_becomes_unhealthy() {
    let mut worker_a = MockWorker::new(MockWorkerConfig {
        port: 20704,
        worker_type: WorkerType::Regular,
        health_status: HealthStatus::Healthy,
        response_delay_ms: 1000,
        fail_rate: 0.0,
    });
    let mut worker_b = MockWorker::new(MockWorkerConfig {
        port: 20705,
        worker_type: WorkerType::Regular,
        health_status: HealthStatus::Healthy,
        response_delay_ms: 1000,
        fail_rate: 0.0,
    });
    let url_a = worker_a.start().await.unwrap();
    let url_b = worker_b.start().await.unwrap();

    let config = RouterConfig {
        mode: RoutingMode::Regular {
            worker_urls: vec![url_a.clone(), url_b.clone()],
        },
        policy: PolicyConfig::ConsistentHash {
            virtual_nodes: 160,
            session_config: SessionAffinityConfig::default(),
        },
        worker_startup_timeout_secs: 1,
        worker_startup_check_interval_secs: 1,
        request_timeout_secs: 10,
        ..Default::default()
    };

    let admission = WorkerAdmissionConfig {
        max_concurrent_requests_per_worker: Some(1),
        worker_queue_size: 1,
        queue_timeout: Duration::ZERO,
    };
    let app_context = common::create_test_context_with_admission(config, admission);
    let router: Arc<dyn RouterTrait> =
        Arc::from(RouterFactory::create_router(&app_context).await.unwrap());

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "x-session-id",
        axum::http::HeaderValue::from_static("failover-session-1"),
    );

    let body = chat_request();
    let first_router = Arc::clone(&router);
    let first_headers = headers.clone();
    let first_body = body.clone();
    let first = tokio::spawn(async move {
        first_router
            .route_chat(Some(&first_headers), &first_body, None)
            .await
    });

    // Wait until the first request is actually occupying one worker's slot.
    let mut busy_worker: Option<String> = None;
    for _ in 0..40 {
        let stats_a = app_context.worker_registry.admission_stats(&url_a);
        let stats_b = app_context.worker_registry.admission_stats(&url_b);
        if stats_a.inflight == 1 {
            busy_worker = Some(url_a.clone());
            break;
        }
        if stats_b.inflight == 1 {
            busy_worker = Some(url_b.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let busy = busy_worker.expect("first request should occupy a worker slot");
    let other = if busy == url_a {
        url_b.clone()
    } else {
        url_a.clone()
    };

    // Second request with the same session queues behind the first one.
    let second_router = Arc::clone(&router);
    let second_headers = headers.clone();
    let second_body = body.clone();
    let second = tokio::spawn(async move {
        second_router
            .route_chat(Some(&second_headers), &second_body, None)
            .await
    });

    // Make sure it is actually waiting in the busy worker's queue, then mark
    // that worker unhealthy while the request is still queued.
    for _ in 0..40 {
        let stats = app_context.worker_registry.admission_stats(&busy);
        if stats.queued == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let busy_worker_arc = app_context
        .worker_registry
        .get_by_url(&busy)
        .expect("busy worker should be registered");
    busy_worker_arc.set_healthy(false);

    let (first_resp, second_resp) = tokio::join!(first, second);
    assert_eq!(first_resp.unwrap().status(), StatusCode::OK);
    assert_eq!(second_resp.unwrap().status(), StatusCode::OK);

    // The queued request must NOT have been sent to the worker that went
    // unhealthy: that worker only saw the first request, and the failover
    // worker saw exactly the retried second request.
    assert_eq!(
        common::mock_worker::get_captured_requests(url_port(&busy)).len(),
        1
    );
    assert_eq!(
        common::mock_worker::get_captured_requests(url_port(&other)).len(),
        1
    );

    worker_a.stop().await;
    worker_b.stop().await;
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
