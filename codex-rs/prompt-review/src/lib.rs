use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

mod gateway;
mod grok_cli;
mod privacy;

pub use gateway::GatewayConfig;
pub use gateway::InvocationError;
pub use gateway::JevInvoker;
pub use gateway::PROMPT_REVIEW_AUDIT_VERSION;
pub use gateway::PromptReviewAudit;
pub use gateway::PromptReviewDecision;
pub use gateway::PromptReviewDisposition;
pub use gateway::PromptReviewGateway;
pub use gateway::PromptReviewRequest;
pub use grok_cli::GrokCliConfig;
pub use grok_cli::GrokCliInvoker;
pub use privacy::ContentRisk;
pub use privacy::EligibilityPolicy;
pub use privacy::PreparedPrompt;
pub use privacy::PromptCategory;
pub use privacy::PromptReviewInput;
pub use privacy::ReviewPreparation;
pub use privacy::SkipReason;
pub use privacy::is_valid_opt_out_reason;
pub use privacy::prepare_review;
pub use privacy::prompt_hash;

pub const JEV_REVIEW_SCHEMA_VERSION: &str = "jev.review.v1";
const MAX_ADVICE_BYTES: usize = 4 * 1024;
const MAX_REASON_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JevOutcome {
    Allow,
    AllowWithAdvice,
    Reject,
    Unavailable,
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JevResponse {
    pub schema_version: String,
    pub outcome: JevOutcome,
    #[schemars(required)]
    pub advice: Option<String>,
    #[schemars(required)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JevParseError {
    MalformedJson,
    UnsupportedVersion,
    InvalidFields,
}

pub fn parse_jev_response(bytes: &[u8]) -> Result<JevResponse, JevParseError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| JevParseError::MalformedJson)?;
    let object = value.as_object().ok_or(JevParseError::MalformedJson)?;
    const REQUIRED_FIELDS: [&str; 4] = ["schema_version", "outcome", "advice", "reason"];
    if object.len() != REQUIRED_FIELDS.len()
        || REQUIRED_FIELDS
            .iter()
            .any(|field| !object.contains_key(*field))
    {
        return Err(JevParseError::MalformedJson);
    }
    let response: JevResponse =
        serde_json::from_value(value).map_err(|_| JevParseError::MalformedJson)?;
    if response.schema_version != JEV_REVIEW_SCHEMA_VERSION {
        return Err(JevParseError::UnsupportedVersion);
    }
    let advice = response.advice.as_deref();
    let reason = response.reason.as_deref();
    let invalid_advice = advice.is_some_and(|value| value.len() > MAX_ADVICE_BYTES);
    let invalid_reason = reason.is_some_and(|value| value.len() > MAX_REASON_BYTES);
    let missing_advice = response.outcome == JevOutcome::AllowWithAdvice
        && advice.is_none_or(|value| value.trim().is_empty());
    let missing_reason = matches!(
        response.outcome,
        JevOutcome::Reject | JevOutcome::Unavailable | JevOutcome::Malformed
    ) && reason.is_none_or(|value| value.trim().is_empty());
    let unexpected_advice = response.outcome != JevOutcome::AllowWithAdvice && advice.is_some();
    let unexpected_reason = matches!(
        response.outcome,
        JevOutcome::Allow | JevOutcome::AllowWithAdvice
    ) && reason.is_some();
    if invalid_advice
        || invalid_reason
        || missing_advice
        || missing_reason
        || unexpected_advice
        || unexpected_reason
    {
        return Err(JevParseError::InvalidFields);
    }
    Ok(response)
}

#[cfg(test)]
#[path = "contract_tests.rs"]
mod contract_tests;

#[cfg(test)]
#[path = "privacy_tests.rs"]
mod privacy_tests;

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod gateway_tests;

#[cfg(test)]
#[path = "grok_cli_tests.rs"]
mod grok_cli_tests;
