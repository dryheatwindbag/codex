use codex_config::config_toml::PromptReviewCategoryToml;
use codex_config::config_toml::PromptReviewConfigToml;
use codex_prompt_review::EligibilityPolicy;
use codex_prompt_review::GatewayConfig;
use codex_prompt_review::GrokCliConfig;
use codex_prompt_review::PromptCategory;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_TIMEOUT_MS: u64 = 20_000;
const DEFAULT_MAX_INPUT_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptReviewConfig {
    pub gateway: GatewayConfig,
    pub cli: GrokCliConfig,
    pub eligibility: EligibilityPolicy,
}

impl Default for PromptReviewConfig {
    fn default() -> Self {
        match resolve_prompt_review_config(None) {
            Ok(config) => config,
            Err(error) => panic!("built-in prompt review defaults must be valid: {error}"),
        }
    }
}

pub(super) fn resolve_prompt_review_config(
    config: Option<PromptReviewConfigToml>,
) -> io::Result<PromptReviewConfig> {
    let config = config.unwrap_or_default();
    let executable = config.executable.unwrap_or_else(|| "grok".to_string());
    if executable.trim().is_empty() {
        return Err(invalid("prompt_review.executable must not be empty"));
    }
    let model = config.model.unwrap_or_else(|| "default".to_string());
    if model.trim().is_empty() {
        return Err(invalid("prompt_review.model must not be empty"));
    }
    let timeout_ms = bounded(
        "prompt_review.timeout_ms",
        config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        100,
        60_000,
    )?;
    let max_input_bytes = bounded(
        "prompt_review.max_input_bytes",
        config.max_input_bytes.unwrap_or(DEFAULT_MAX_INPUT_BYTES),
        1,
        1024 * 1024,
    )?;
    let max_output_bytes = bounded(
        "prompt_review.max_output_bytes",
        config.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
        1,
        1024 * 1024,
    )?;
    let max_attempts = bounded(
        "prompt_review.max_attempts",
        config.max_attempts.unwrap_or(1),
        1,
        3,
    )?;
    let max_concurrency = bounded(
        "prompt_review.max_concurrency",
        config.max_concurrency.unwrap_or(4),
        1,
        32,
    )?;
    if config.exact_redactions.len() > 128
        || config
            .exact_redactions
            .iter()
            .any(|value| !(3..=1024).contains(&value.len()))
    {
        return Err(invalid(
            "prompt_review.exact_redactions must contain at most 128 values of 3-1024 bytes each",
        ));
    }
    let blocking_categories = config
        .blocking_categories
        .into_iter()
        .map(map_category)
        .collect::<HashSet<_>>();
    Ok(PromptReviewConfig {
        gateway: GatewayConfig {
            enabled: config.enabled.unwrap_or(false),
            max_attempts,
            max_concurrency,
            blocking_categories,
        },
        cli: GrokCliConfig {
            executable: PathBuf::from(executable),
            model,
            timeout: Duration::from_millis(timeout_ms),
            max_output_bytes,
        },
        eligibility: EligibilityPolicy {
            max_input_bytes,
            exact_redactions: config.exact_redactions,
        },
    })
}

fn bounded<T>(field: &str, value: T, minimum: T, maximum: T) -> io::Result<T>
where
    T: Copy + PartialOrd + std::fmt::Display,
{
    if value < minimum || value > maximum {
        return Err(invalid(format!(
            "{field} must be between {minimum} and {maximum}; got {value}"
        )));
    }
    Ok(value)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn map_category(category: PromptReviewCategoryToml) -> PromptCategory {
    match category {
        PromptReviewCategoryToml::RootUser => PromptCategory::RootUser,
        PromptReviewCategoryToml::Orchestration => PromptCategory::Orchestration,
        PromptReviewCategoryToml::SubagentInitial => PromptCategory::SubagentInitial,
        PromptReviewCategoryToml::SubagentFollowup => PromptCategory::SubagentFollowup,
        PromptReviewCategoryToml::Retry => PromptCategory::Retry,
        PromptReviewCategoryToml::Review => PromptCategory::Review,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_config::config_toml::PromptReviewCategoryToml;
    use codex_config::config_toml::PromptReviewConfigToml;
    use codex_prompt_review::PromptCategory;
    use pretty_assertions::assert_eq;

    #[test]
    fn defaults_to_disabled_advisory_review() {
        let config = resolve_prompt_review_config(None).expect("default config");

        assert!(!config.gateway.enabled);
        assert!(config.gateway.blocking_categories.is_empty());
        assert_eq!(config.cli.timeout.as_secs(), 20);
        assert_eq!(config.eligibility.max_input_bytes, 32 * 1024);
    }

    #[test]
    fn resolves_explicit_bounded_settings() {
        let config = resolve_prompt_review_config(Some(PromptReviewConfigToml {
            enabled: Some(true),
            executable: Some("/opt/grok".to_string()),
            model: Some("grok-review".to_string()),
            timeout_ms: Some(1_500),
            max_input_bytes: Some(4_096),
            max_output_bytes: Some(8_192),
            max_attempts: Some(2),
            max_concurrency: Some(3),
            blocking_categories: vec![PromptReviewCategoryToml::Review],
            exact_redactions: vec!["private-id".to_string()],
        }))
        .expect("valid config");

        assert!(config.gateway.enabled);
        assert_eq!(config.cli.executable.to_string_lossy(), "/opt/grok");
        assert_eq!(config.cli.model, "grok-review");
        assert_eq!(config.cli.timeout.as_millis(), 1_500);
        assert_eq!(config.gateway.max_attempts, 2);
        assert_eq!(config.gateway.max_concurrency, 3);
        assert_eq!(
            config.gateway.blocking_categories,
            [PromptCategory::Review].into_iter().collect()
        );
        assert_eq!(config.eligibility.exact_redactions, vec!["private-id"]);
    }

    #[test]
    fn rejects_unbounded_or_empty_settings() {
        for config in [
            PromptReviewConfigToml {
                timeout_ms: Some(0),
                ..Default::default()
            },
            PromptReviewConfigToml {
                max_attempts: Some(4),
                ..Default::default()
            },
            PromptReviewConfigToml {
                max_concurrency: Some(0),
                ..Default::default()
            },
            PromptReviewConfigToml {
                executable: Some("  ".to_string()),
                ..Default::default()
            },
            PromptReviewConfigToml {
                exact_redactions: vec!["x".to_string()],
                ..Default::default()
            },
        ] {
            assert!(resolve_prompt_review_config(Some(config)).is_err());
        }
    }
}
