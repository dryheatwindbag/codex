use crate::EligibilityPolicy;
use crate::JevOutcome;
use crate::PreparedPrompt;
use crate::PromptCategory;
use crate::PromptReviewInput;
use crate::ReviewPreparation;
use crate::parse_jev_response;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::OnceCell;
use tokio::sync::Semaphore;

pub const PROMPT_REVIEW_AUDIT_VERSION: &str = "prompt_review.audit.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayConfig {
    pub enabled: bool,
    pub max_attempts: usize,
    pub max_concurrency: usize,
    pub blocking_categories: HashSet<PromptCategory>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_attempts: 1,
            max_concurrency: 4,
            blocking_categories: HashSet::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvocationError {
    Unavailable(String),
    Timeout,
    OutputTooLarge,
    MalformedEnvelope(String),
    ProcessFailure(String),
}

impl InvocationError {
    fn reason(&self) -> String {
        match self {
            Self::Unavailable(reason) => format!("unavailable: {reason}"),
            Self::Timeout => "timeout".to_string(),
            Self::OutputTooLarge => "oversized_response".to_string(),
            Self::MalformedEnvelope(reason) => format!("malformed_envelope: {reason}"),
            // Process diagnostics can echo the reviewed prompt. Keep them out
            // of persistent audit metadata and expose only a stable code.
            Self::ProcessFailure(_) => "process_failure".to_string(),
        }
    }

    fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout | Self::ProcessFailure(_))
    }

    fn is_malformed(&self) -> bool {
        matches!(self, Self::OutputTooLarge | Self::MalformedEnvelope(_))
    }
}

pub trait JevInvoker: Send + Sync {
    fn model(&self) -> &str;
    fn version(&self) -> &str;
    fn invoke<'a>(
        &'a self,
        prompt: &'a PreparedPrompt,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, InvocationError>> + Send + 'a>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptReviewDisposition {
    Disabled,
    Skipped,
    Allow,
    AllowWithAdvice,
    RejectAdvisory,
    RejectBlocking,
    UnavailableAdvisory,
    UnavailableBlocking,
    MalformedAdvisory,
    MalformedBlocking,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptReviewAudit {
    pub schema_version: String,
    pub review_id: String,
    pub prompt_hash: String,
    pub prompt_category: PromptCategory,
    pub jev_model: String,
    pub jev_version: String,
    pub timestamp_unix_ms: u64,
    pub disposition: PromptReviewDisposition,
    pub latency_ms: u64,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptReviewDecision {
    pub should_block: bool,
    pub advice: Option<String>,
    pub audit: PromptReviewAudit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptReviewRequest<'a> {
    pub input: PromptReviewInput<'a>,
    pub review_depth: u8,
    /// Stable identity for the owning thread and turn, never prompt text.
    pub scope_id: &'a str,
}

pub struct PromptReviewGateway {
    config: GatewayConfig,
    eligibility: EligibilityPolicy,
    invoker: Arc<dyn JevInvoker>,
    semaphore: Semaphore,
    decisions: Mutex<HashMap<String, Arc<OnceCell<PromptReviewDecision>>>>,
}

impl PromptReviewGateway {
    pub fn new(
        config: GatewayConfig,
        eligibility: EligibilityPolicy,
        invoker: Arc<dyn JevInvoker>,
    ) -> Self {
        let max_concurrency = config.max_concurrency.max(1);
        Self {
            config,
            eligibility,
            invoker,
            semaphore: Semaphore::new(max_concurrency),
            decisions: Mutex::new(HashMap::new()),
        }
    }

    pub fn remember_audits<'a>(&self, audits: impl IntoIterator<Item = &'a PromptReviewAudit>) {
        let mut decisions = self
            .decisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for audit in audits {
            let cell = Arc::new(OnceCell::new());
            let _ = cell.set(decision_from_audit(audit.clone()));
            decisions.entry(audit.review_id.clone()).or_insert(cell);
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub async fn review(&self, request: PromptReviewRequest<'_>) -> PromptReviewDecision {
        let started = Instant::now();
        let category = request.input.category;
        let scope_id = request.scope_id;
        let preparation = crate::prepare_review(request.input, &self.eligibility);
        let (prompt_hash, prepared, skip_reason) = match preparation {
            ReviewPreparation::Eligible(prepared) => {
                (prepared.prompt_hash.clone(), Some(prepared), None)
            }
            ReviewPreparation::Skipped {
                prompt_hash,
                reason,
                detail,
                ..
            } => (
                prompt_hash,
                None,
                Some(match detail {
                    Some(detail) => format!("{reason:?}: {detail}"),
                    None => format!("{reason:?}"),
                }),
            ),
        };
        let review_id = stable_review_id(scope_id, category, &prompt_hash);

        if !self.config.enabled {
            return self.decision(
                review_id,
                prompt_hash,
                category,
                PromptReviewDisposition::Disabled,
                false,
                None,
                None,
                started,
            );
        }
        if request.review_depth > 0 {
            return self.decision(
                review_id,
                prompt_hash,
                category,
                PromptReviewDisposition::Skipped,
                false,
                None,
                Some("recursion".to_string()),
                started,
            );
        }
        let Some(prepared) = prepared else {
            return self.decision(
                review_id,
                prompt_hash,
                category,
                PromptReviewDisposition::Skipped,
                false,
                None,
                skip_reason,
                started,
            );
        };
        let decision_cell = self
            .decisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(review_id.clone())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        decision_cell
            .get_or_init(|| async {
                self.review_prepared(review_id, prompt_hash, category, prepared, started)
                    .await
            })
            .await
            .clone()
    }

    async fn review_prepared(
        &self,
        review_id: String,
        prompt_hash: String,
        category: PromptCategory,
        prepared: PreparedPrompt,
        started: Instant,
    ) -> PromptReviewDecision {
        let Ok(_permit) = self.semaphore.acquire().await else {
            return self.failure_decision(
                review_id,
                prompt_hash,
                category,
                false,
                "gateway_closed".to_string(),
                started,
            );
        };
        let mut attempt = 0usize;
        let response = loop {
            attempt += 1;
            match self.invoker.invoke(&prepared).await {
                Ok(bytes) => break Ok(bytes),
                Err(error) if error.is_retryable() && attempt < self.config.max_attempts.max(1) => {
                    continue;
                }
                Err(error) => break Err(error),
            }
        };
        match response {
            Ok(bytes) => match parse_jev_response(&bytes) {
                Ok(response) => {
                    self.response_decision(review_id, prompt_hash, category, response, started)
                }
                Err(error) => self.failure_decision(
                    review_id,
                    prompt_hash,
                    category,
                    true,
                    format!("parse_error: {error:?}"),
                    started,
                ),
            },
            Err(error) => self.failure_decision(
                review_id,
                prompt_hash,
                category,
                error.is_malformed(),
                error.reason(),
                started,
            ),
        }
    }

    fn response_decision(
        &self,
        review_id: String,
        prompt_hash: String,
        category: PromptCategory,
        response: crate::JevResponse,
        started: Instant,
    ) -> PromptReviewDecision {
        let blocking = self.config.blocking_categories.contains(&category);
        let (disposition, should_block, failure_reason) = match response.outcome {
            JevOutcome::Allow => (PromptReviewDisposition::Allow, false, None),
            JevOutcome::AllowWithAdvice => (PromptReviewDisposition::AllowWithAdvice, false, None),
            JevOutcome::Reject if blocking => (
                PromptReviewDisposition::RejectBlocking,
                true,
                Some("jev_reject".to_string()),
            ),
            JevOutcome::Reject => (
                PromptReviewDisposition::RejectAdvisory,
                false,
                Some("jev_reject".to_string()),
            ),
            JevOutcome::Unavailable if blocking => (
                PromptReviewDisposition::UnavailableBlocking,
                true,
                Some("jev_unavailable".to_string()),
            ),
            JevOutcome::Unavailable => (
                PromptReviewDisposition::UnavailableAdvisory,
                false,
                Some("jev_unavailable".to_string()),
            ),
            JevOutcome::Malformed if blocking => (
                PromptReviewDisposition::MalformedBlocking,
                true,
                Some("jev_malformed".to_string()),
            ),
            JevOutcome::Malformed => (
                PromptReviewDisposition::MalformedAdvisory,
                false,
                Some("jev_malformed".to_string()),
            ),
        };
        self.decision(
            review_id,
            prompt_hash,
            category,
            disposition,
            should_block,
            response.advice,
            failure_reason,
            started,
        )
    }

    fn failure_decision(
        &self,
        review_id: String,
        prompt_hash: String,
        category: PromptCategory,
        malformed: bool,
        failure_reason: String,
        started: Instant,
    ) -> PromptReviewDecision {
        let blocking = self.config.blocking_categories.contains(&category);
        let disposition = match (malformed, blocking) {
            (true, true) => PromptReviewDisposition::MalformedBlocking,
            (true, false) => PromptReviewDisposition::MalformedAdvisory,
            (false, true) => PromptReviewDisposition::UnavailableBlocking,
            (false, false) => PromptReviewDisposition::UnavailableAdvisory,
        };
        self.decision(
            review_id,
            prompt_hash,
            category,
            disposition,
            blocking,
            None,
            Some(failure_reason),
            started,
        )
    }

    #[expect(clippy::too_many_arguments)]
    fn decision(
        &self,
        review_id: String,
        prompt_hash: String,
        category: PromptCategory,
        disposition: PromptReviewDisposition,
        should_block: bool,
        advice: Option<String>,
        failure_reason: Option<String>,
        started: Instant,
    ) -> PromptReviewDecision {
        let timestamp_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        PromptReviewDecision {
            should_block,
            advice,
            audit: PromptReviewAudit {
                schema_version: PROMPT_REVIEW_AUDIT_VERSION.to_string(),
                review_id,
                prompt_hash,
                prompt_category: category,
                jev_model: self.invoker.model().to_string(),
                jev_version: self.invoker.version().to_string(),
                timestamp_unix_ms,
                disposition,
                latency_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                failure_reason: failure_reason.map(|reason| reason.chars().take(1_024).collect()),
            },
        }
    }
}

fn decision_from_audit(audit: PromptReviewAudit) -> PromptReviewDecision {
    let should_block = matches!(
        audit.disposition,
        PromptReviewDisposition::RejectBlocking
            | PromptReviewDisposition::UnavailableBlocking
            | PromptReviewDisposition::MalformedBlocking
    );
    PromptReviewDecision {
        should_block,
        advice: None,
        audit,
    }
}

fn stable_review_id(scope_id: &str, category: PromptCategory, prompt_hash: &str) -> String {
    let category = match category {
        PromptCategory::RootUser => "root_user",
        PromptCategory::Orchestration => "orchestration",
        PromptCategory::SubagentInitial => "subagent_initial",
        PromptCategory::SubagentFollowup => "subagent_followup",
        PromptCategory::Retry => "retry",
        PromptCategory::Review => "review",
    };
    format!(
        "sha256:{:x}",
        Sha256::digest(format!("{scope_id}\0{category}\0{prompt_hash}").as_bytes())
    )
}
