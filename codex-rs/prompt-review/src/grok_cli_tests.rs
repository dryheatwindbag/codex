use super::*;
use crate::grok_cli::extract_review_payload;
use crate::grok_cli::read_bounded;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

fn prepared() -> PreparedPrompt {
    PreparedPrompt {
        prompt_hash: "sha256:test".to_string(),
        category: PromptCategory::RootUser,
        redacted_text: "safe prompt".to_string(),
    }
}

#[test]
fn extracts_review_json_from_grok_envelope() {
    let inner =
        r#"{"schema_version":"jev.review.v1","outcome":"allow","advice":null,"reason":null}"#;
    let outer = serde_json::json!({"text": inner, "session_id": "not-audit-data"});

    assert_eq!(
        extract_review_payload(outer.to_string().as_bytes()),
        Ok(inner.as_bytes().to_vec())
    );
}

#[test]
fn rejects_grok_envelope_without_text() {
    assert!(matches!(
        extract_review_payload(br#"{"session_id":"missing"}"#),
        Err(InvocationError::MalformedEnvelope(_))
    ));
}

#[tokio::test]
async fn missing_grok_cli_is_unavailable() {
    let invoker = GrokCliInvoker::new(GrokCliConfig {
        executable: PathBuf::from("definitely-not-a-real-jev-command-42"),
        timeout: Duration::from_secs(1),
        ..Default::default()
    });

    let result = invoker.invoke(&prepared()).await;

    assert!(matches!(result, Err(InvocationError::Unavailable(_))));
}

#[tokio::test]
async fn bounded_reader_drains_but_never_buffers_past_the_limit() {
    let (mut writer, reader) = tokio::io::duplex(64);
    let task = tokio::spawn(async move { read_bounded(reader, 8).await });
    writer
        .write_all(b"0123456789abcdef")
        .await
        .expect("fixture write");
    drop(writer);

    let output = task.await.expect("reader task").expect("bounded read");

    assert_eq!(output.bytes, b"01234567");
    assert!(output.overflowed);
}

#[cfg(unix)]
fn executable_script(body: &str) -> (tempfile::TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("fixture directory");
    let path = directory.path().join("fake-grok");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("fixture script");
    let mut permissions = std::fs::metadata(&path)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions).expect("fixture permissions");
    (directory, path)
}

#[cfg(unix)]
#[tokio::test]
async fn kills_timed_out_grok_process() {
    let (directory, executable) = executable_script(
        "sleep 5 &\nprintf '%s' \"$!\" > \"$(dirname \"$0\")/descendant.pid\"\nwait",
    );
    let invoker = GrokCliInvoker::new(GrokCliConfig {
        executable,
        timeout: Duration::from_millis(20),
        ..Default::default()
    });

    let result = tokio::time::timeout(Duration::from_secs(1), invoker.invoke(&prepared()))
        .await
        .expect("runner must return after its deadline");

    assert_eq!(result, Err(InvocationError::Timeout));

    let descendant_pid: libc::pid_t =
        std::fs::read_to_string(directory.path().join("descendant.pid"))
            .expect("descendant pid")
            .parse()
            .expect("numeric descendant pid");
    let mut gone = false;
    for _ in 0..20 {
        if unsafe { libc::kill(descendant_pid, 0) } == -1 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "timed-out Jev descendant must be terminated");
}

#[cfg(unix)]
#[tokio::test]
async fn bounds_output_drain_when_a_descendant_keeps_the_pipe_open() {
    let body = r#"
sleep 5 &
printf '%s' "$!" > "$(dirname "$0")/descendant.pid"
printf '%s\n' '{"text":"{\"schema_version\":\"jev.review.v1\",\"outcome\":\"allow\",\"advice\":null,\"reason\":null}"}'
"#;
    let (directory, executable) = executable_script(body);
    let invoker = GrokCliInvoker::new(GrokCliConfig {
        executable,
        timeout: Duration::from_millis(20),
        ..Default::default()
    });

    let bounded = tokio::time::timeout(Duration::from_secs(1), invoker.invoke(&prepared())).await;
    if bounded.is_err()
        && let Ok(pid) = std::fs::read_to_string(directory.path().join("descendant.pid"))
            .expect("descendant pid")
            .parse::<libc::pid_t>()
    {
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let result = bounded.expect("runner must apply its deadline to output draining");

    assert_eq!(result, Err(InvocationError::Timeout));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_oversized_cli_output() {
    let (_directory, executable) = executable_script("printf '0123456789abcdef'");
    let invoker = GrokCliInvoker::new(GrokCliConfig {
        executable,
        max_output_bytes: 8,
        ..Default::default()
    });

    let result = invoker.invoke(&prepared()).await;

    assert_eq!(result, Err(InvocationError::OutputTooLarge));
}

#[cfg(unix)]
#[tokio::test]
async fn successful_cli_invocation_returns_only_the_strict_review_payload() {
    let body = r#"
if [ -n "$OPENAI_API_KEY" ] || [ -n "$XAI_API_KEY" ]; then
  exit 17
fi
printf '%s\n' '{"text":"{\"schema_version\":\"jev.review.v1\",\"outcome\":\"allow\",\"advice\":null,\"reason\":null}","session_id":"discard-me"}'
"#;
    let (_directory, executable) = executable_script(body);
    let invoker = GrokCliInvoker::new(GrokCliConfig {
        executable,
        timeout: Duration::from_secs(1),
        ..Default::default()
    });

    let payload = invoker
        .invoke(&prepared())
        .await
        .expect("valid fake Grok response");

    assert_eq!(
        parse_jev_response(&payload)
            .expect("strict review payload")
            .outcome,
        JevOutcome::Allow
    );
}
