use super::*;
use codex_history::ResponseItemEnvelope;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;

fn text_input(text: &str) -> TurnInput {
    TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
        acceptance_order: None,
    }
}

#[test]
fn classifies_root_generated_retry_and_review_prompts() {
    for (input, trigger, expected) in [
        (text_input("root"), None, PromptCategory::RootUser),
        (
            text_input("generated"),
            Some("goal"),
            PromptCategory::Orchestration,
        ),
        (text_input("retry"), Some("retry"), PromptCategory::Retry),
        (text_input("review"), Some("review"), PromptCategory::Review),
    ] {
        assert_eq!(
            candidate_for_input(&input, &SessionSource::Exec, trigger, false)
                .expect("eligible prompt")
                .category,
            expected
        );
    }
}

#[test]
fn classifies_nested_subagent_initial_and_followup_prompts() {
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 2,
        agent_nickname: Some("worker".to_string()),
        agent_role: None,
        agent_path: None,
    });

    assert_eq!(
        candidate_for_input(&text_input("initial"), &source, None, false)
            .expect("initial prompt")
            .category,
        PromptCategory::SubagentInitial
    );
    assert_eq!(
        candidate_for_input(&text_input("follow up"), &source, None, true)
            .expect("followup prompt")
            .category,
        PromptCategory::SubagentFollowup
    );
}

#[test]
fn reviews_interagent_and_generated_message_dispatches() {
    let mail =
        TurnInput::InterAgentCommunication(codex_protocol::protocol::InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::root(),
            Vec::new(),
            "continue the task".to_string(),
            true,
        ));
    let generated = TurnInput::ResponseItem(ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "generated orchestration".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }));

    for input in [mail, generated] {
        assert_eq!(
            candidate_for_input(&input, &SessionSource::Exec, None, false)
                .expect("eligible orchestration prompt")
                .category,
            PromptCategory::Orchestration
        );
    }
}

#[test]
fn excludes_tool_outputs_internal_sessions_and_realtime() {
    let tool_output = TurnInput::FunctionCallOutput(ResponseItemEnvelope::new(ResponseItem::Other));
    assert!(candidate_for_input(&tool_output, &SessionSource::Exec, None, false).is_none());
    assert!(
        candidate_for_input(
            &text_input("compaction"),
            &SessionSource::Internal(
                codex_protocol::protocol::InternalSessionSource::MemoryConsolidation
            ),
            None,
            false,
        )
        .is_none()
    );
    assert!(
        candidate_for_input(
            &text_input("audio transcript"),
            &SessionSource::Exec,
            Some("realtime"),
            false,
        )
        .is_none()
    );
}

#[test]
fn excludes_mixed_media_and_encrypted_prompts_before_export() {
    let mixed = TurnInput::UserInput {
        content: vec![
            UserInput::Text {
                text: "caption".to_string(),
                text_elements: Vec::new(),
            },
            UserInput::Audio {
                audio_url: "data:audio/wav;base64,private".to_string(),
            },
        ],
        client_id: None,
        acceptance_order: None,
    };
    assert_eq!(
        candidate_for_input(&mixed, &SessionSource::Exec, None, false)
            .expect("audited exclusion")
            .content_risk,
        ContentRisk::ContainsNonText
    );

    let encrypted = TurnInput::InterAgentCommunication(
        codex_protocol::protocol::InterAgentCommunication::new_encrypted(
            AgentPath::root(),
            AgentPath::root(),
            Vec::new(),
            "ciphertext".to_string(),
            true,
        ),
    );
    assert_eq!(
        candidate_for_input(&encrypted, &SessionSource::Exec, None, false)
            .expect("audited exclusion")
            .content_risk,
        ContentRisk::PrivateSource
    );
}

#[test]
fn reviews_text_without_exporting_skill_or_mention_paths() {
    let input = TurnInput::UserInput {
        content: vec![
            UserInput::Text {
                text: "perform the bounded task".to_string(),
                text_elements: Vec::new(),
            },
            UserInput::Skill {
                name: "private-skill".to_string(),
                path: "/private/skill/SKILL.md".into(),
            },
            UserInput::Mention {
                name: "private-app".to_string(),
                path: "app://private-connector".to_string(),
            },
        ],
        client_id: None,
        acceptance_order: None,
    };

    assert_eq!(
        candidate_for_input(&input, &SessionSource::Exec, None, false),
        Some(PromptCandidate {
            text: "perform the bounded task".to_string(),
            category: PromptCategory::RootUser,
            content_risk: ContentRisk::TextOnly,
            review_depth: 0,
        })
    );
}

#[test]
fn audits_structured_only_input_as_ineligible_without_exporting_paths() {
    let input = TurnInput::UserInput {
        content: vec![UserInput::Skill {
            name: "private-skill".to_string(),
            path: "/private/skill/SKILL.md".into(),
        }],
        client_id: None,
        acceptance_order: None,
    };

    assert_eq!(
        candidate_for_input(&input, &SessionSource::Exec, None, false),
        Some(PromptCandidate {
            text: "[non-text prompt]".to_string(),
            category: PromptCategory::RootUser,
            content_risk: ContentRisk::ContainsNonText,
            review_depth: 0,
        })
    );
}

#[test]
fn prompt_opt_out_is_one_shot_and_bound_to_the_matching_input() {
    let registry = PromptReviewOptOutRegistry::default();
    let first = text_input("first prompt");
    let second = text_input("second prompt");

    registry.register(&second, "incompatible_reviewer");

    assert_eq!(registry.take(&first), None);
    assert_eq!(
        registry.take(&second).as_deref(),
        Some("incompatible_reviewer")
    );
    assert_eq!(registry.take(&second), None);
}

#[test]
fn prompt_opt_out_does_not_transfer_between_structured_prompts_with_the_same_text() {
    let registry = PromptReviewOptOutRegistry::default();
    let with_skill = |path: &str| TurnInput::UserInput {
        content: vec![
            UserInput::Text {
                text: "same text".to_string(),
                text_elements: Vec::new(),
            },
            UserInput::Skill {
                name: "skill".to_string(),
                path: path.into(),
            },
        ],
        client_id: None,
        acceptance_order: None,
    };
    let first = with_skill("/private/one/SKILL.md");
    let second = with_skill("/private/two/SKILL.md");

    registry.register(&first, "incompatible_reviewer");

    assert_eq!(registry.take(&second), None);
    assert_eq!(
        registry.take(&first).as_deref(),
        Some("incompatible_reviewer")
    );
}

#[test]
fn advisory_failures_and_blocking_dispositions_are_visible() {
    use crate::session::turn::prompt_review_disposition_warning;

    let advisory = prompt_review_disposition_warning(
        codex_prompt_review::PromptReviewDisposition::UnavailableAdvisory,
        Some("timeout"),
    )
    .expect("advisory failure warning");
    assert!(advisory.contains("execution will continue"));
    assert!(advisory.contains("timeout"));

    let blocking = prompt_review_disposition_warning(
        codex_prompt_review::PromptReviewDisposition::RejectBlocking,
        Some("policy"),
    )
    .expect("blocking warning");
    assert!(blocking.contains("blocked this prompt"));
    assert!(blocking.contains("policy"));

    assert_eq!(
        prompt_review_disposition_warning(
            codex_prompt_review::PromptReviewDisposition::Allow,
            None
        ),
        None
    );
}

#[cfg(unix)]
fn fake_jev_cli(
    response: codex_prompt_review::JevResponse,
) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = tempfile::tempdir().expect("fake Jev tempdir");
    let executable = temp_dir.path().join("fake-grok");
    let calls = temp_dir.path().join("calls.log");
    let captured_prompt = temp_dir.path().join("captured-prompt.txt");
    let inner = serde_json::to_string(&response).expect("serialize Jev response");
    let outer = serde_json::json!({ "text": inner }).to_string();
    let script = format!(
        r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--prompt-file" ]; then
    shift
    cp "$1" '{}'
  fi
  shift
done
printf 'call\n' >> '{}'
printf '%s\n' '{}'
"#,
        captured_prompt.display(),
        calls.display(),
        outer
    );
    std::fs::write(&executable, script).expect("write fake Jev CLI");
    let mut permissions = std::fs::metadata(&executable)
        .expect("fake Jev metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).expect("make fake Jev executable");
    (temp_dir, executable, calls)
}

#[cfg(unix)]
#[tokio::test]
async fn central_dispatch_reviews_once_and_preserves_the_original_prompt() {
    use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
    use crate::session::turn::run_hooks_and_record_inputs;
    use codex_login::CodexAuth;
    use codex_prompt_review::JEV_REVIEW_SCHEMA_VERSION;
    use codex_prompt_review::JevOutcome;
    use codex_thread_store::PersistContext;

    let original = "keep  exact spacing and punctuation!?";
    let (fake_dir, executable, calls) = fake_jev_cli(codex_prompt_review::JevResponse {
        schema_version: JEV_REVIEW_SCHEMA_VERSION.to_string(),
        outcome: JevOutcome::AllowWithAdvice,
        advice: Some("retain the original prompt".to_string()),
        reason: None,
    });
    let captured_prompt = fake_dir.path().join("captured-prompt.txt");
    let (session, turn_context, events) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| {
            config.prompt_review.gateway.enabled = true;
            config.prompt_review.cli.executable = executable.clone();
        },
    )
    .await;
    let input = vec![text_input(original)];
    let model_info = turn_context.capture_current_model_info();

    assert!(
        !run_hooks_and_record_inputs(
            &session,
            &turn_context,
            &model_info,
            &input,
            PersistContext::Standard,
        )
        .await
    );
    assert!(
        !run_hooks_and_record_inputs(
            &session,
            &turn_context,
            &model_info,
            &input,
            PersistContext::Standard,
        )
        .await
    );

    assert_eq!(
        std::fs::read_to_string(&calls).expect("read fake CLI calls"),
        "call\n"
    );
    assert!(
        std::fs::read_to_string(captured_prompt)
            .expect("read fake CLI prompt")
            .contains(original)
    );
    let history = session
        .clone_history()
        .await
        .raw_items()
        .cloned()
        .collect::<Vec<_>>();
    let user_texts = history
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                content.first().and_then(|part| match part {
                    ContentItem::InputText { text } => Some(text.as_str()),
                    _ => None,
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(user_texts, vec![original, original]);
    let mut saw_advice = false;
    while let Ok(event) = events.try_recv() {
        saw_advice |= matches!(
            event.msg,
            codex_protocol::protocol::EventMsg::Warning(
                codex_protocol::protocol::WarningEvent { ref message }
            ) if message.contains("retain the original prompt")
        );
    }
    assert!(saw_advice);
}

#[cfg(unix)]
#[tokio::test]
async fn configured_blocking_disposition_stops_only_the_rejected_prompt() {
    use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
    use crate::session::turn::run_hooks_and_record_inputs;
    use codex_login::CodexAuth;
    use codex_prompt_review::JEV_REVIEW_SCHEMA_VERSION;
    use codex_prompt_review::JevOutcome;
    use codex_thread_store::PersistContext;

    let (_fake_dir, executable, _calls) = fake_jev_cli(codex_prompt_review::JevResponse {
        schema_version: JEV_REVIEW_SCHEMA_VERSION.to_string(),
        outcome: JevOutcome::Reject,
        advice: None,
        reason: Some("configured policy category".to_string()),
    });
    let (session, turn_context, events) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| {
            config.prompt_review.gateway.enabled = true;
            config.prompt_review.cli.executable = executable.clone();
            config
                .prompt_review
                .gateway
                .blocking_categories
                .insert(PromptCategory::RootUser);
        },
    )
    .await;

    assert!(
        run_hooks_and_record_inputs(
            &session,
            &turn_context,
            &turn_context.capture_current_model_info(),
            &[text_input("reject me")],
            PersistContext::Standard,
        )
        .await
    );
    assert!(session.clone_history().await.raw_items().next().is_none());
    let mut saw_blocked = false;
    while let Ok(event) = events.try_recv() {
        saw_blocked |= matches!(
            event.msg,
            codex_protocol::protocol::EventMsg::Warning(
                codex_protocol::protocol::WarningEvent { ref message }
            ) if message.contains("blocked this prompt")
        );
    }
    assert!(saw_blocked);
}
