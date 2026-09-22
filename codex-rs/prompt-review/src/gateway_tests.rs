use super::*;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[derive(Default)]
struct FakeInvoker {
    responses: Mutex<VecDeque<Result<Vec<u8>, InvocationError>>>,
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    delay: Duration,
}

impl FakeInvoker {
    fn with(responses: impl IntoIterator<Item = Result<Vec<u8>, InvocationError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            ..Default::default()
        }
    }

    fn allowing(delay: Duration) -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            delay,
            ..Default::default()
        }
    }
}

impl JevInvoker for FakeInvoker {
    fn model(&self) -> &str {
        "grok-test"
    }

    fn version(&self) -> &str {
        "test-v1"
    }

    fn invoke<'a>(
        &'a self,
        _prompt: &'a PreparedPrompt,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, InvocationError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.responses
                .lock()
                .expect("fake response lock")
                .pop_front()
                .unwrap_or_else(|| Ok(allow_response()))
        })
    }
}

fn allow_response() -> Vec<u8> {
    br#"{"schema_version":"jev.review.v1","outcome":"allow","advice":null,"reason":null}"#.to_vec()
}

fn request(text: &str, category: PromptCategory) -> PromptReviewRequest<'_> {
    PromptReviewRequest {
        input: PromptReviewInput {
            category,
            text,
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: None,
        },
        review_depth: 0,
        scope_id: "thread-1:turn-1",
    }
}

#[tokio::test]
async fn disabled_gateway_never_invokes_jev() {
    let invoker = Arc::new(FakeInvoker::default());
    let gateway = PromptReviewGateway::new(
        GatewayConfig {
            enabled: false,
            ..Default::default()
        },
        EligibilityPolicy::default(),
        invoker.clone(),
    );

    let decision = gateway
        .review(request("do the task", PromptCategory::RootUser))
        .await;

    assert_eq!(
        decision.audit.disposition,
        PromptReviewDisposition::Disabled
    );
    assert!(!decision.should_block);
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn advice_is_separate_and_the_original_prompt_is_unchanged() {
    let original = String::from("  preserve me exactly\n");
    let before = original.clone();
    let invoker = Arc::new(FakeInvoker::with([Ok(
        br#"{"schema_version":"jev.review.v1","outcome":"allow_with_advice","advice":"add a test","reason":null}"#.to_vec(),
    )]));
    let gateway = PromptReviewGateway::new(
        GatewayConfig {
            enabled: true,
            ..Default::default()
        },
        EligibilityPolicy::default(),
        invoker,
    );

    let decision = gateway
        .review(request(&original, PromptCategory::Orchestration))
        .await;

    assert_eq!(original, before);
    assert_eq!(decision.advice.as_deref(), Some("add a test"));
    assert_eq!(
        decision.audit.disposition,
        PromptReviewDisposition::AllowWithAdvice
    );
    assert!(!decision.should_block);
}

#[tokio::test]
async fn reject_is_advisory_unless_the_category_is_blocking() {
    let rejected = || {
        Ok(
            br#"{"schema_version":"jev.review.v1","outcome":"reject","advice":null,"reason":"unsafe scope"}"#.to_vec(),
        )
    };
    for (blocking, expected_disposition, should_block) in [
        (false, PromptReviewDisposition::RejectAdvisory, false),
        (true, PromptReviewDisposition::RejectBlocking, true),
    ] {
        let mut config = GatewayConfig {
            enabled: true,
            ..Default::default()
        };
        if blocking {
            config.blocking_categories.insert(PromptCategory::Review);
        }
        let gateway = PromptReviewGateway::new(
            config,
            EligibilityPolicy::default(),
            Arc::new(FakeInvoker::with([rejected()])),
        );

        let decision = gateway
            .review(request("review", PromptCategory::Review))
            .await;

        assert_eq!(decision.audit.disposition, expected_disposition);
        assert_eq!(decision.should_block, should_block);
    }
}

#[tokio::test]
async fn unavailable_and_malformed_never_invent_allow() {
    for (response, expected) in [
        (
            Err(InvocationError::Unavailable(
                "missing executable".to_string(),
            )),
            PromptReviewDisposition::UnavailableAdvisory,
        ),
        (
            Ok(b"not json".to_vec()),
            PromptReviewDisposition::MalformedAdvisory,
        ),
    ] {
        let gateway = PromptReviewGateway::new(
            GatewayConfig {
                enabled: true,
                ..Default::default()
            },
            EligibilityPolicy::default(),
            Arc::new(FakeInvoker::with([response])),
        );

        let decision = gateway.review(request("task", PromptCategory::Retry)).await;

        assert_eq!(decision.audit.disposition, expected);
        assert!(!decision.should_block);
        assert!(decision.audit.failure_reason.is_some());
    }
}

#[tokio::test]
async fn process_stderr_never_enters_persistent_failure_metadata() {
    let gateway = PromptReviewGateway::new(
        GatewayConfig {
            enabled: true,
            ..Default::default()
        },
        EligibilityPolicy::default(),
        Arc::new(FakeInvoker::with([Err(InvocationError::ProcessFailure(
            "echoed private prompt body".to_string(),
        ))])),
    );

    let decision = gateway
        .review(request("private prompt body", PromptCategory::RootUser))
        .await;

    assert_eq!(
        decision.audit.failure_reason.as_deref(),
        Some("process_failure")
    );
}

#[tokio::test]
async fn jev_reason_never_enters_persistent_failure_metadata() {
    for (outcome, expected_reason) in [
        ("reject", "jev_reject"),
        ("unavailable", "jev_unavailable"),
        ("malformed", "jev_malformed"),
    ] {
        let response = serde_json::json!({
            "schema_version": "jev.review.v1",
            "outcome": outcome,
            "advice": null,
            "reason": "echoed private prompt body",
        })
        .to_string()
        .into_bytes();
        let gateway = PromptReviewGateway::new(
            GatewayConfig {
                enabled: true,
                ..Default::default()
            },
            EligibilityPolicy::default(),
            Arc::new(FakeInvoker::with([Ok(response)])),
        );

        let decision = gateway
            .review(request("private prompt body", PromptCategory::RootUser))
            .await;

        assert_eq!(
            decision.audit.failure_reason.as_deref(),
            Some(expected_reason),
            "{outcome}"
        );
    }
}

#[tokio::test]
async fn retries_only_retryable_invocation_failures() {
    let invoker = Arc::new(FakeInvoker::with([
        Err(InvocationError::Timeout),
        Ok(allow_response()),
    ]));
    let gateway = PromptReviewGateway::new(
        GatewayConfig {
            enabled: true,
            max_attempts: 2,
            ..Default::default()
        },
        EligibilityPolicy::default(),
        invoker.clone(),
    );

    let decision = gateway
        .review(request("retry me", PromptCategory::Retry))
        .await;

    assert_eq!(decision.audit.disposition, PromptReviewDisposition::Allow);
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn deduplicates_already_processed_prompts() {
    let invoker = Arc::new(FakeInvoker::default());
    let gateway = PromptReviewGateway::new(
        GatewayConfig {
            enabled: true,
            ..Default::default()
        },
        EligibilityPolicy::default(),
        invoker.clone(),
    );

    let first = gateway
        .review(request("same prompt", PromptCategory::SubagentFollowup))
        .await;
    let second = gateway
        .review(request("same prompt", PromptCategory::SubagentFollowup))
        .await;

    assert_eq!(first.audit.disposition, PromptReviewDisposition::Allow);
    assert_eq!(second.audit.disposition, PromptReviewDisposition::Allow);
    assert_eq!(first.audit.review_id, second.audit.review_id);
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn identical_text_in_a_new_turn_is_reviewed_again() {
    let invoker = Arc::new(FakeInvoker::default());
    let gateway = PromptReviewGateway::new(
        GatewayConfig {
            enabled: true,
            ..Default::default()
        },
        EligibilityPolicy::default(),
        invoker.clone(),
    );
    let first = request("same prompt", PromptCategory::RootUser);
    let mut second = request("same prompt", PromptCategory::RootUser);
    second.scope_id = "thread-1:turn-2";

    gateway.review(first).await;
    gateway.review(second).await;

    assert_eq!(invoker.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn restored_review_metadata_prevents_reinvocation_and_preserves_blocking() {
    let original_invoker = Arc::new(FakeInvoker::with([Ok(
        br#"{"schema_version":"jev.review.v1","outcome":"reject","advice":null,"reason":"policy"}"#
            .to_vec(),
    )]));
    let config = GatewayConfig {
        enabled: true,
        blocking_categories: [PromptCategory::Review].into_iter().collect(),
        ..Default::default()
    };
    let original = PromptReviewGateway::new(
        config.clone(),
        EligibilityPolicy::default(),
        original_invoker,
    )
    .review(request("review", PromptCategory::Review))
    .await;
    assert!(original.should_block);

    let resumed_invoker = Arc::new(FakeInvoker::default());
    let resumed = PromptReviewGateway::new(
        config,
        EligibilityPolicy::default(),
        resumed_invoker.clone(),
    );
    resumed.remember_audits([&original.audit]);

    let decision = resumed
        .review(request("review", PromptCategory::Review))
        .await;

    assert!(decision.should_block);
    assert_eq!(decision.audit, original.audit);
    assert_eq!(resumed_invoker.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn bounds_concurrent_reviews() {
    let invoker = Arc::new(FakeInvoker::allowing(Duration::from_millis(30)));
    let gateway = Arc::new(PromptReviewGateway::new(
        GatewayConfig {
            enabled: true,
            max_concurrency: 2,
            ..Default::default()
        },
        EligibilityPolicy::default(),
        invoker.clone(),
    ));
    let mut tasks = Vec::new();
    for index in 0..6 {
        let gateway = Arc::clone(&gateway);
        tasks.push(tokio::spawn(async move {
            let text = format!("prompt {index}");
            gateway
                .review(request(&text, PromptCategory::SubagentInitial))
                .await
        }));
    }
    for task in tasks {
        task.await.expect("review task should join");
    }

    assert_eq!(invoker.max_active.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn review_recursion_is_skipped_without_invocation() {
    let invoker = Arc::new(FakeInvoker::default());
    let gateway = PromptReviewGateway::new(
        GatewayConfig {
            enabled: true,
            ..Default::default()
        },
        EligibilityPolicy::default(),
        invoker.clone(),
    );
    let mut recursive = request("review the reviewer", PromptCategory::Review);
    recursive.review_depth = 1;

    let decision = gateway.review(recursive).await;

    assert_eq!(decision.audit.disposition, PromptReviewDisposition::Skipped);
    assert_eq!(decision.audit.failure_reason.as_deref(), Some("recursion"));
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn concurrent_duplicate_waits_for_and_reuses_the_blocking_decision() {
    let invoker = Arc::new(FakeInvoker {
        responses: Mutex::new(VecDeque::from([Ok(
            br#"{"schema_version":"jev.review.v1","outcome":"reject","advice":null,"reason":"policy"}"#.to_vec(),
        )])),
        delay: Duration::from_millis(30),
        ..Default::default()
    });
    let gateway = Arc::new(PromptReviewGateway::new(
        GatewayConfig {
            enabled: true,
            blocking_categories: [PromptCategory::RootUser].into_iter().collect(),
            ..Default::default()
        },
        EligibilityPolicy::default(),
        invoker.clone(),
    ));
    let first_gateway = Arc::clone(&gateway);
    let second_gateway = Arc::clone(&gateway);
    let first = tokio::spawn(async move {
        first_gateway
            .review(request("same blocking prompt", PromptCategory::RootUser))
            .await
    });
    let second = tokio::spawn(async move {
        second_gateway
            .review(request("same blocking prompt", PromptCategory::RootUser))
            .await
    });

    let first = first.await.expect("first review task");
    let second = second.await.expect("second review task");

    assert!(first.should_block);
    assert!(second.should_block);
    assert_eq!(first.audit.review_id, second.audit.review_id);
    assert_eq!(invoker.calls.load(Ordering::SeqCst), 1);
}
