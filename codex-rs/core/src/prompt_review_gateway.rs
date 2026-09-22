use crate::config::PromptReviewConfig;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_prompt_review::ContentRisk;
use codex_prompt_review::GrokCliInvoker;
use codex_prompt_review::PromptCategory;
use codex_prompt_review::PromptReviewDecision;
use codex_prompt_review::PromptReviewGateway;
use codex_prompt_review::PromptReviewInput;
use codex_prompt_review::PromptReviewRequest;
use codex_prompt_review::prompt_hash;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

#[derive(Clone)]
pub(crate) struct SharedPromptReviewGateway(pub(crate) Arc<PromptReviewGateway>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptInputKind {
    User,
    ResponseItem,
    InterAgent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptReviewOptOutEntry {
    prompt_hash: String,
    input_kind: PromptInputKind,
    reason: String,
}

/// One-shot opt-outs keyed to the exact prompt bytes and input kind.
///
/// This lives on a turn context so steering and turn-start dispatches share the
/// same path without broadening an opt-out to unrelated prompts in that turn.
#[derive(Debug, Default)]
pub(crate) struct PromptReviewOptOutRegistry(Mutex<VecDeque<PromptReviewOptOutEntry>>);

impl PromptReviewOptOutRegistry {
    pub(crate) fn register(&self, input: &TurnInput, reason: impl Into<String>) -> bool {
        let Some((prompt_hash, input_kind)) = prompt_identity(input) else {
            return false;
        };
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(PromptReviewOptOutEntry {
                prompt_hash,
                input_kind,
                reason: reason.into(),
            });
        true
    }

    pub(crate) fn take(&self, input: &TurnInput) -> Option<String> {
        let (prompt_hash, input_kind) = prompt_identity(input)?;
        let mut entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = entries
            .iter()
            .position(|entry| entry.prompt_hash == prompt_hash && entry.input_kind == input_kind)?;
        entries.remove(index).map(|entry| entry.reason)
    }
}

pub(crate) fn register_prompt_review_opt_out(
    turn_context: &TurnContext,
    input: &TurnInput,
    reason: impl Into<String>,
) -> bool {
    turn_context
        .extension_data
        .get_or_init(PromptReviewOptOutRegistry::default)
        .register(input, reason)
}

impl SharedPromptReviewGateway {
    pub(crate) fn new(config: &PromptReviewConfig) -> Self {
        let invoker = Arc::new(GrokCliInvoker::new(config.cli.clone()));
        Self(Arc::new(PromptReviewGateway::new(
            config.gateway.clone(),
            config.eligibility.clone(),
            invoker,
        )))
    }
}

pub(crate) async fn review_pending_input(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    input: &TurnInput,
    input_index: usize,
) -> Option<PromptReviewDecision> {
    if crate::guardian::is_basic_session_source(&turn_context.session_source) {
        return None;
    }
    if !sess.services.prompt_review_gateway.is_enabled() {
        return None;
    }
    let trigger = turn_context.turn_metadata_state.current_turn_trigger();
    let seen_subagent = sess
        .services
        .prompt_review_seen_subagent_prompt
        .load(Ordering::Relaxed);
    let candidate = candidate_for_input(
        input,
        &turn_context.session_source,
        trigger.as_deref(),
        seen_subagent,
    )?;
    if matches!(
        candidate.category,
        PromptCategory::SubagentInitial | PromptCategory::SubagentFollowup
    ) {
        sess.services
            .prompt_review_seen_subagent_prompt
            .store(true, Ordering::Relaxed);
    }
    let opt_out_reason = turn_context
        .extension_data
        .get::<PromptReviewOptOutRegistry>()
        .and_then(|registry| registry.take(input));
    let scope_id = format!("{}:{}:{input_index}", sess.thread_id, turn_context.sub_id);
    Some(
        sess.services
            .prompt_review_gateway
            .review(PromptReviewRequest {
                input: PromptReviewInput {
                    category: candidate.category,
                    text: &candidate.text,
                    content_risk: candidate.content_risk,
                    opt_out_reason: opt_out_reason.as_deref(),
                },
                review_depth: candidate.review_depth,
                scope_id: &scope_id,
            })
            .await,
    )
}

fn prompt_identity(input: &TurnInput) -> Option<(String, PromptInputKind)> {
    candidate_for_input(input, &SessionSource::Exec, None, false)?;
    let (serialized, input_kind) = match input {
        TurnInput::UserInput { content, .. } => {
            (serde_json::to_string(content).ok()?, PromptInputKind::User)
        }
        TurnInput::ResponseItem(envelope) => (
            serde_json::to_string(&envelope.item).ok()?,
            PromptInputKind::ResponseItem,
        ),
        TurnInput::InterAgentCommunication(message) => (
            serde_json::to_string(message).ok()?,
            PromptInputKind::InterAgent,
        ),
        TurnInput::FunctionCallOutput(_) => return None,
    };
    Some((prompt_hash(&serialized), input_kind))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptCandidate {
    pub(crate) text: String,
    pub(crate) category: PromptCategory,
    pub(crate) content_risk: ContentRisk,
    pub(crate) review_depth: u8,
}

pub(crate) fn candidate_for_input(
    input: &TurnInput,
    session_source: &SessionSource,
    turn_trigger: Option<&str>,
    subagent_has_prior_prompt: bool,
) -> Option<PromptCandidate> {
    if matches!(session_source, SessionSource::Internal(_))
        || matches!(turn_trigger, Some("realtime" | "memory_consolidation"))
    {
        return None;
    }
    let category = category_for(
        input,
        session_source,
        turn_trigger,
        subagent_has_prior_prompt,
    );
    let review_depth = u8::from(turn_trigger == Some("jev_prompt_review"));
    match input {
        TurnInput::UserInput { content, .. } => {
            user_input_candidate(content, category, review_depth)
        }
        TurnInput::InterAgentCommunication(message) => {
            let (text, content_risk) = match message.encrypted_content.as_deref() {
                Some(encrypted) => (encrypted.to_string(), ContentRisk::PrivateSource),
                None => (message.content.clone(), ContentRisk::TextOnly),
            };
            (!text.trim().is_empty()).then_some(PromptCandidate {
                text,
                category,
                content_risk,
                review_depth,
            })
        }
        TurnInput::ResponseItem(envelope) => {
            response_item_candidate(&envelope.item, category, review_depth)
        }
        TurnInput::FunctionCallOutput(_) => None,
    }
}

fn category_for(
    input: &TurnInput,
    session_source: &SessionSource,
    turn_trigger: Option<&str>,
    subagent_has_prior_prompt: bool,
) -> PromptCategory {
    if turn_trigger == Some("review") {
        return PromptCategory::Review;
    }
    if turn_trigger == Some("retry") {
        return PromptCategory::Retry;
    }
    if matches!(
        session_source,
        SessionSource::SubAgent(SubAgentSource::Review)
    ) {
        return PromptCategory::Review;
    }
    if matches!(session_source, SessionSource::SubAgent(_)) {
        return if subagent_has_prior_prompt {
            PromptCategory::SubagentFollowup
        } else {
            PromptCategory::SubagentInitial
        };
    }
    if !matches!(input, TurnInput::UserInput { .. })
        || turn_trigger.is_some_and(|trigger| trigger != "user")
    {
        PromptCategory::Orchestration
    } else {
        PromptCategory::RootUser
    }
}

fn user_input_candidate(
    content: &[UserInput],
    category: PromptCategory,
    review_depth: u8,
) -> Option<PromptCandidate> {
    let mut text = Vec::new();
    let mut contains_non_text = false;
    let mut contains_structured_metadata = false;
    for item in content {
        match item {
            UserInput::Text {
                text: value,
                text_elements: _,
            } => {
                text.push(value.as_str());
            }
            UserInput::Image { .. }
            | UserInput::LocalImage { .. }
            | UserInput::Audio { .. }
            | UserInput::LocalAudio { .. } => contains_non_text = true,
            // Skill and mention paths are local metadata. Review the adjacent
            // task text without exporting either path.
            UserInput::Skill { .. } | UserInput::Mention { .. } => {
                contains_structured_metadata = true;
            }
            // UserInput is non-exhaustive across crates. Future variants fail
            // closed as non-text until they receive an explicit policy.
            _ => contains_non_text = true,
        }
    }
    let text_is_empty = text.is_empty();
    if text_is_empty && !contains_non_text && !contains_structured_metadata {
        return None;
    }
    Some(PromptCandidate {
        text: if text_is_empty {
            "[non-text prompt]".to_string()
        } else {
            text.join("\n")
        },
        category,
        content_risk: if contains_non_text || text_is_empty {
            ContentRisk::ContainsNonText
        } else {
            ContentRisk::TextOnly
        },
        review_depth,
    })
}

fn response_item_candidate(
    item: &ResponseItem,
    category: PromptCategory,
    review_depth: u8,
) -> Option<PromptCandidate> {
    let (text, content_risk) = match item {
        ResponseItem::Message { role, content, .. }
            if matches!(role.as_str(), "user" | "developer" | "system") =>
        {
            let mut text = Vec::new();
            let mut non_text = false;
            for item in content {
                match item {
                    ContentItem::InputText { text: value }
                    | ContentItem::OutputText { text: value } => text.push(value.as_str()),
                    ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => {
                        non_text = true;
                    }
                }
            }
            (
                text.join("\n"),
                if non_text {
                    ContentRisk::ContainsNonText
                } else {
                    ContentRisk::TextOnly
                },
            )
        }
        ResponseItem::AgentMessage { content, .. } => {
            let mut text = Vec::new();
            for part in content {
                match part {
                    AgentMessageInputContent::InputText { text: value } => {
                        text.push(value.as_str())
                    }
                    AgentMessageInputContent::EncryptedContent { encrypted_content } => {
                        return Some(PromptCandidate {
                            text: encrypted_content.clone(),
                            category,
                            content_risk: ContentRisk::PrivateSource,
                            review_depth,
                        });
                    }
                }
            }
            (text.join("\n"), ContentRisk::TextOnly)
        }
        _ => return None,
    };
    (!text.trim().is_empty() || content_risk == ContentRisk::ContainsNonText).then_some(
        PromptCandidate {
            text: if text.is_empty() {
                "[non-text prompt]".to_string()
            } else {
                text
            },
            category,
            content_risk,
            review_depth,
        },
    )
}

#[cfg(test)]
#[path = "prompt_review_gateway_tests.rs"]
mod tests;
