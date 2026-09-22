# Jev prompt-review gateway

The Jev gateway is an experimental, disabled-by-default review layer for task prompts. It uses a
headless Grok CLI process to review an eligible, redacted copy of a prompt before Core records or
dispatches the original prompt. The original prompt is never rewritten.

Enabling the gateway does not enable enforcement. All categories remain advisory unless an
administrator separately lists a category in `blocking_categories`.

## Architecture and path inventory

All model-visible pending task inputs converge at
`core/src/session/turn.rs::run_hooks_and_record_inputs`. That function invokes the shared gateway
before `hook_runtime::record_pending_input`. Accepted prompts carry their stable review receipt in
the existing rollout envelope metadata. A prompt stopped before recording receives a standalone,
model-invisible receipt. On resume, Core restores those receipt IDs so the same prompt scope is not
reviewed again.

| Prompt class | Entry or producer | Gateway category |
| --- | --- | --- |
| Root start and steer | `core/src/session/turn_input.rs` | `root_user` |
| App-server and queued root input | app-server turn and queue processors, then Core turn input | `root_user` |
| Generated task context | `TurnInput::ResponseItem`, extensions, and orchestrators | `orchestration` |
| Initial nested-agent task | multi-agent spawn and delivery through Core | `subagent_initial` |
| Agent message or follow-up | multi-agent delivery and mailbox wake through Core | `subagent_followup` |
| Explicit recovery or retry prompt | Core turn trigger `retry` | `retry` |
| Built-in or delegated review prompt | `core/src/session/review.rs` and review subagent source | `review` |

Before this gateway, only `TurnInput::UserInput` reached `UserPromptSubmit` hooks.
`InterAgentCommunication`, generated `ResponseItem` prompts, and the direct built-in review task
bypassed that hook. The generic hook runner also has a different privacy and response contract; it
is not used as the Jev transport.

Transport retries occur after input has been reviewed and recorded, so they reuse the attached
receipt. Concurrent duplicate dispatches wait for and reuse the same decision, including a
blocking decision. A stable review ID combines thread/turn/input scope, prompt category, and the
SHA-256 hash of the original prompt bytes.

The following inputs are intentionally ineligible rather than bypasses:

- tool outputs and sampling continuations, which are not new task prompts;
- compaction and memory-consolidation sessions, which can contain accumulated private history;
- realtime audio/transcript turns;
- encrypted inter-agent content and mixed text/image or text/audio input;
- structured-only skill and mention inputs, which have no reviewable text.

When a prompt combines plain text with a skill or mention, Jev reviews only the text. Skill and
mention paths are never exported.

The classifier exhaustively matches the internal `TurnInput` variants. Adding a variant therefore
requires an explicit eligibility decision before Core compiles.

## Privacy boundary

The reviewer receives only a category, a redacted byte length, and the redacted text from one
pending prompt. It never receives conversation history, repository files, attachment contents,
tool output, the working directory, transcript paths, user identity, or the local prompt hash.

Before invoking Grok, the gateway deterministically:

- excludes non-text, encrypted/private-source, NUL/binary, and oversized input;
- excludes private keys, privilege-marked legal material, probable raw private documents, and
  repository diffs or patch dumps;
- redacts supported credential assignments, bearer/API tokens, known GitHub and Slack tokens,
  URL credentials, and configured exact private values;
- rejects rather than truncates an oversized prompt.

The Grok process runs from a private empty temporary directory with no tools, subagents, or web
search. The prompt is supplied in a private temporary file rather than an argument. The child gets
only the minimum runtime environment (`PATH`, home, and temporary-directory variables), not the
caller's token environment. Timeout cleanup terminates the process group on Unix. Standard output
and error are drained through byte-capped readers. On Windows, the child is assigned to a Job
Object so timeout cleanup terminates its process tree rather than only the direct child.

Redaction is defense in depth, not a proof that arbitrary prose is public. Producers should use
the per-prompt opt-out for prohibited or incompatible content.

## Response and outcome policy

Jev must return `jev.review.v1` JSON with exactly these fields:

```json
{
  "schema_version": "jev.review.v1",
  "outcome": "allow",
  "advice": null,
  "reason": null
}
```

The fields are required, unknown fields are rejected, and outcome-specific field combinations are
validated. Advice is capped at 4096 bytes and reasons at 1024 bytes.

| Outcome | Default advisory handling | Explicitly blocking category |
| --- | --- | --- |
| `allow` | Execute the original prompt unchanged | Same |
| `allow_with_advice` | Execute unchanged and show advice separately | Same |
| `reject` | Warn, audit, and execute unchanged | Stop before recording or sampling |
| `unavailable` | Warn, audit the failure, and execute unchanged | Stop; never synthesize approval |
| `malformed` | Warn, audit the parse/size failure, and execute unchanged | Stop; never synthesize approval |

Ineligible input and explicit opt-outs do not invoke Grok. They execute unchanged and produce a
`skipped` audit disposition. Disabled configuration does not run classification, invoke Grok, or
emit review metadata, preserving pre-gateway production behavior.

## Configuration

```toml
[prompt_review]
enabled = false
executable = "/absolute/path/to/grok"
model = "grok-review-model"
timeout_ms = 20000
max_input_bytes = 32768
max_output_bytes = 16384
max_attempts = 1
max_concurrency = 4
blocking_categories = []
exact_redactions = ["private-project-code"]
```

Bounds enforced during configuration loading:

| Setting | Allowed range | Default |
| --- | ---: | ---: |
| `timeout_ms` | 100–60000 | 20000 |
| `max_input_bytes` | 1–1048576 | 32768 |
| `max_output_bytes` | 1–1048576 | 16384 |
| `max_attempts` | 1–3 | 1 |
| `max_concurrency` | 1–32 | 4 |

`exact_redactions` accepts at most 128 literal values, each 3–1024 bytes long. Shorter values are
rejected because replacing them could destroy unrelated reviewer text.

`blocking_categories` accepts `root_user`, `orchestration`, `subagent_initial`,
`subagent_followup`, `retry`, and `review`. Keep it empty during advisory rollout. Configured model
and executable values are recorded transparently; `model = "default"` means the Grok CLI selects
its own default model.

The typed per-prompt opt-out is
`TurnStartOptions::with_prompt_review_opt_out("reason_code")`. It is one-shot and bound to the
exact prompt bytes and input kind, including when steering an active turn. Reason codes must contain
1–64 ASCII letters, numbers, `.`, `-`, or `_`; unsafe free-form reasons are never copied into audit
metadata. Prompt text is not parsed for opt-out commands.

## Audit record

Audit metadata contains no prompt body or Jev advice. It is attached to the already persisted
prompt envelope when the prompt is accepted; blocked prompts use a standalone model-invisible
receipt.

```json
{
  "schema_version": "prompt_review.audit.v1",
  "review_id": "sha256:9f6b...",
  "prompt_hash": "sha256:49c1...",
  "prompt_category": "subagent_followup",
  "jev_model": "grok-review-model",
  "jev_version": "jev.review.v1",
  "timestamp_unix_ms": 1790146800000,
  "disposition": "allow_with_advice",
  "latency_ms": 842,
  "failure_reason": null
}
```

`jev_version` identifies the enforced Jev contract/adapter version. CLI runtime version should be
captured separately in deployment inventory. Jev-authored `reason` text is never persisted;
outcomes map to bounded stable failure codes. Process stderr and other transport details are also
never persisted because they could echo prompt content.

## Rollout and rollback

1. Merge with `enabled = false`; no system-wide review or enforcement starts from this change.
2. In a local canary, set `enabled = true` and keep `blocking_categories = []`.
3. Inspect latency, exclusion, unavailable, malformed, and advisory-reject rates.
4. Expand canary scope gradually. Authorize any blocking category as a separate policy change.
5. Roll back immediately by setting `enabled = false`. Reverting the implementation is also safe:
   the metadata field is additive and no state migration or destructive cleanup is required.

## Verification

Run focused checks from the repository root:

```sh
just test -p codex-prompt-review
just test -p codex-config prompt_review
just test -p codex-history prompt_review
just test -p codex-rollout prompt_review
just test -p codex-core prompt_review
just write-config-schema
git diff --check
```

Repository policy requires separate authorization before running the complete workspace
`just test`. Record exact focused results in the draft pull request.
