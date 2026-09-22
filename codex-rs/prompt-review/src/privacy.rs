use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::sync::LazyLock;

static SENSITIVE_LEGAL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r"(?i)\b(attorney[- ]client|attorney work product|privileged legal|legal privilege|work[- ]product privilege)\b",
    )
});
static PRIVATE_DOCUMENT_FIELD_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r"(?i)\b(name|date of birth|social security(?: number)?|medical record|patient id|account number|case number|case no\.)\s*:",
    )
});
static HIGH_CONFIDENCE_PRIVATE_DATA_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r"(?i)\b\d{3}-\d{2}-\d{4}\b|\b(?:mrn|medical\s+record(?:\s+number)?|patient\s+id|account(?:\s+(?:number|no\.?))?|acct)\s*[:#]\s*[a-z0-9-]{4,}\b|\b(?:date\s+of\s+birth|dob)\s*:\s*\d{1,4}[-/]\d{1,2}[-/]\d{1,4}\b",
    )
});
static PRIVATE_KEY_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"(?i)-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----"));
static KNOWN_TOKEN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,})\b")
});
static URL_CREDENTIAL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"(?i)(https?://)[^/@\s]+:[^/@\s]+@"));
static EXPLICIT_CREDENTIAL_ASSIGNMENT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(
        r#"(?i)\b(api[_-]?key|access[_-]?token|auth[_-]?token|token|secret|password|passwd|credential)s?\b(\s*[:=]\s*)(["']?)[^\s,"'}\]]+"#,
    )
});
static EXPLICIT_BEARER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"(?i)\bBearer[ \t]+[^\s,;]+"));

fn compile_regex(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => panic!("invalid prompt-review regex `{pattern}`: {error}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptCategory {
    RootUser,
    Orchestration,
    SubagentInitial,
    SubagentFollowup,
    Retry,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRisk {
    TextOnly,
    ContainsNonText,
    PrivateSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptReviewInput<'a> {
    pub category: PromptCategory,
    pub text: &'a str,
    pub content_risk: ContentRisk,
    pub opt_out_reason: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EligibilityPolicy {
    pub max_input_bytes: usize,
    pub exact_redactions: Vec<String>,
}

impl Default for EligibilityPolicy {
    fn default() -> Self {
        Self {
            max_input_bytes: 32 * 1024,
            exact_redactions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPrompt {
    pub prompt_hash: String,
    pub category: PromptCategory,
    pub redacted_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    ExplicitOptOut,
    NonText,
    PrivateSource,
    SensitiveCredential,
    SensitiveLegalMaterial,
    ProbablePrivateDocument,
    RepositoryContent,
    Oversized,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewPreparation {
    Eligible(PreparedPrompt),
    Skipped {
        prompt_hash: String,
        category: PromptCategory,
        reason: SkipReason,
        detail: Option<String>,
    },
}

pub fn prepare_review(
    input: PromptReviewInput<'_>,
    policy: &EligibilityPolicy,
) -> ReviewPreparation {
    let prompt_hash = prompt_hash(input.text);
    if let Some(reason) = input.opt_out_reason {
        return ReviewPreparation::Skipped {
            prompt_hash,
            category: input.category,
            reason: SkipReason::ExplicitOptOut,
            detail: Some(if is_valid_opt_out_reason(reason) {
                reason.to_string()
            } else {
                "redacted_invalid_reason".to_string()
            }),
        };
    }
    let risk_reason = match input.content_risk {
        ContentRisk::TextOnly => None,
        ContentRisk::ContainsNonText => Some(SkipReason::NonText),
        ContentRisk::PrivateSource => Some(SkipReason::PrivateSource),
    };
    if let Some(reason) = risk_reason {
        return ReviewPreparation::Skipped {
            prompt_hash,
            category: input.category,
            reason,
            detail: None,
        };
    }
    if input.text.contains('\0') {
        return ReviewPreparation::Skipped {
            prompt_hash,
            category: input.category,
            reason: SkipReason::Binary,
            detail: None,
        };
    }
    if input.text.len() > policy.max_input_bytes {
        return ReviewPreparation::Skipped {
            prompt_hash,
            category: input.category,
            reason: SkipReason::Oversized,
            detail: None,
        };
    }
    if PRIVATE_KEY_REGEX.is_match(input.text) {
        return ReviewPreparation::Skipped {
            prompt_hash,
            category: input.category,
            reason: SkipReason::SensitiveCredential,
            detail: None,
        };
    }
    if SENSITIVE_LEGAL_REGEX.is_match(input.text) {
        return ReviewPreparation::Skipped {
            prompt_hash,
            category: input.category,
            reason: SkipReason::SensitiveLegalMaterial,
            detail: None,
        };
    }
    if HIGH_CONFIDENCE_PRIVATE_DATA_REGEX.is_match(input.text) {
        return ReviewPreparation::Skipped {
            prompt_hash,
            category: input.category,
            reason: SkipReason::ProbablePrivateDocument,
            detail: None,
        };
    }
    if input.text.lines().count() >= 3
        && PRIVATE_DOCUMENT_FIELD_REGEX.find_iter(input.text).count() >= 2
    {
        return ReviewPreparation::Skipped {
            prompt_hash,
            category: input.category,
            reason: SkipReason::ProbablePrivateDocument,
            detail: None,
        };
    }
    if contains_repository_dump(input.text) {
        return ReviewPreparation::Skipped {
            prompt_hash,
            category: input.category,
            reason: SkipReason::RepositoryContent,
            detail: None,
        };
    }
    let mut redacted_text = EXPLICIT_CREDENTIAL_ASSIGNMENT_REGEX
        .replace_all(input.text, "$1$2$3[REDACTED_SECRET]")
        .into_owned();
    redacted_text = EXPLICIT_BEARER_REGEX
        .replace_all(&redacted_text, "Bearer [REDACTED_SECRET]")
        .into_owned();
    redacted_text = codex_secrets::redact_secrets(redacted_text);
    redacted_text = KNOWN_TOKEN_REGEX
        .replace_all(&redacted_text, "[REDACTED_SECRET]")
        .into_owned();
    redacted_text = URL_CREDENTIAL_REGEX
        .replace_all(&redacted_text, "$1[REDACTED_SECRET]@")
        .into_owned();
    let mut exact_redactions = policy
        .exact_redactions
        .iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    exact_redactions.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for value in exact_redactions {
        redacted_text = redacted_text.replace(value, "[REDACTED_PRIVATE]");
    }
    ReviewPreparation::Eligible(PreparedPrompt {
        prompt_hash,
        category: input.category,
        redacted_text,
    })
}

fn contains_repository_dump(text: &str) -> bool {
    text.lines().any(|line| {
        line.starts_with("diff --git ") || line == "*** Begin Patch" || line.starts_with("Index: ")
    }) || (text.contains("\n--- a/") && text.contains("\n+++ b/"))
}

/// Opt-out reasons are audit codes, not free-form text that could leak secrets.
pub fn is_valid_opt_out_reason(reason: &str) -> bool {
    !reason.is_empty()
        && reason.len() <= 64
        && reason
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Hashes the original prompt bytes without normalizing or retaining prompt text.
pub fn prompt_hash(text: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}
