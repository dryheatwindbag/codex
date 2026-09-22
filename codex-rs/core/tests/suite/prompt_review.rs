use codex_core::TurnInputRequest;
use codex_history::RolloutItem;
use codex_prompt_review::PromptCategory;
use codex_prompt_review::PromptReviewAudit;
use codex_prompt_review::PromptReviewDisposition;
use codex_protocol::protocol::EventMsg;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::os::unix::fs::PermissionsExt;

fn fake_jev_cli() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("fake Jev directory");
    let executable = directory.path().join("fake-grok");
    let calls = directory.path().join("calls.log");
    let script = format!(
        r#"#!/bin/sh
printf 'call\n' >> '{}'
printf '%s\n' '{{"text":"{{\"schema_version\":\"jev.review.v1\",\"outcome\":\"allow\",\"advice\":null,\"reason\":null}}"}}'
"#,
        calls.display()
    );
    std::fs::write(&executable, script).expect("write fake Jev CLI");
    let mut permissions = std::fs::metadata(&executable)
        .expect("fake Jev metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&executable, permissions).expect("make fake Jev executable");
    (directory, executable, calls)
}

async fn persisted_review_audits(
    codex: &codex_core::CodexThread,
) -> anyhow::Result<Vec<PromptReviewAudit>> {
    codex.shutdown_and_wait().await?;
    let rollout_path = codex.rollout_path().expect("rollout path");
    let history = codex_rollout::RolloutRecorder::get_rollout_history(&rollout_path).await?;
    Ok(history
        .get_rollout_items()
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => envelope
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.prompt_review.clone()),
            _ => None,
        })
        .collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_prompt_passes_through_jev_once_without_mutation() -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let response = responses::mount_sse_once(&server, responses::sse_completed("done")).await;
    let (_fake_dir, executable, calls) = fake_jev_cli();
    let test = test_codex()
        .with_config(move |config| {
            config.prompt_review.gateway.enabled = true;
            config.prompt_review.cli.executable = executable;
        })
        .build_with_auto_env(&server)
        .await?;
    let original = "preserve  exact spacing and punctuation!?";

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: original.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert_eq!(
        std::fs::read_to_string(calls)?,
        "call\n",
        "the central gate must invoke Jev exactly once"
    );
    let user_messages = response.single_request().message_input_texts("user");
    assert_eq!(
        user_messages
            .iter()
            .filter(|message| message.as_str() == original)
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![original]
    );
    let audits = persisted_review_audits(&test.codex).await?;
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].prompt_category, PromptCategory::RootUser);
    assert_eq!(audits[0].disposition, PromptReviewDisposition::Allow);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_prompt_opt_out_is_audited_without_invoking_jev() -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let response = responses::mount_sse_once(&server, responses::sse_completed("done")).await;
    let (_fake_dir, executable, calls) = fake_jev_cli();
    let test = test_codex()
        .with_config(move |config| {
            config.prompt_review.gateway.enabled = true;
            config.prompt_review.cli.executable = executable;
        })
        .build_with_auto_env(&server)
        .await?;
    let prompt = "do not export this compatible text";
    let request = TurnInputRequest::user_input(vec![UserInput::Text {
        text: prompt.to_string(),
        text_elements: Vec::new(),
    }])
    .on_start(TurnStartOptions::default().with_prompt_review_opt_out("private_source"));

    test.codex.start_or_steer_turn(request).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert!(
        !calls.exists(),
        "one-shot opt-out must avoid Jev invocation"
    );
    let user_messages = response.single_request().message_input_texts("user");
    assert_eq!(
        user_messages
            .iter()
            .filter(|message| message.as_str() == prompt)
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![prompt]
    );
    let audits = persisted_review_audits(&test.codex).await?;
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].prompt_category, PromptCategory::RootUser);
    assert_eq!(audits[0].disposition, PromptReviewDisposition::Skipped);
    assert_eq!(
        audits[0].failure_reason.as_deref(),
        Some("ExplicitOptOut: private_source")
    );
    Ok(())
}
