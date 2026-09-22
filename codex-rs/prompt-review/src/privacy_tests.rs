use super::*;

#[test]
fn prompt_hash_is_stable_and_byte_exact() {
    assert_eq!(
        prompt_hash("keep  spacing\n"),
        "sha256:7d201efb48072ce694dfe2f42998a5b4670bc6978d80fb3ff503482411e96be2"
    );
    assert_ne!(
        prompt_hash("keep  spacing\n"),
        prompt_hash("keep spacing\n")
    );
}

#[test]
fn hashes_the_exact_original_prompt_without_mutating_it() {
    let original = String::from("Review this exact prompt.\nKeep whitespace.  ");
    let before = original.clone();

    let ReviewPreparation::Eligible(prepared) = prepare_review(
        PromptReviewInput {
            category: PromptCategory::RootUser,
            text: &original,
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: None,
        },
        &EligibilityPolicy::default(),
    ) else {
        panic!("text prompt should be eligible");
    };

    assert_eq!(original, before);
    assert_eq!(prepared.prompt_hash.len(), "sha256:".len() + 64);
    assert!(prepared.prompt_hash.starts_with("sha256:"));
}

#[test]
fn redacts_secrets_and_exact_private_values_from_reviewer_text_only() {
    let original = String::from(
        "Use Bearer abcdefghijklmnopqrstuvwxyz and api_key=abcdefgh12345678 for ACME-MATTER-7",
    );
    let policy = EligibilityPolicy {
        exact_redactions: vec!["ACME-MATTER-7".to_string()],
        ..EligibilityPolicy::default()
    };

    let ReviewPreparation::Eligible(prepared) = prepare_review(
        PromptReviewInput {
            category: PromptCategory::Orchestration,
            text: &original,
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: None,
        },
        &policy,
    ) else {
        panic!("redactable prompt should be eligible");
    };

    assert_eq!(
        prepared.redacted_text,
        "Use Bearer [REDACTED_SECRET] and api_key=[REDACTED_SECRET] for [REDACTED_PRIVATE]"
    );
    assert!(original.contains("ACME-MATTER-7"));
}

#[test]
fn records_explicit_per_prompt_opt_out_without_exporting_text() {
    let preparation = prepare_review(
        PromptReviewInput {
            category: PromptCategory::Review,
            text: "private prompt body",
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: Some("privileged_legal_material"),
        },
        &EligibilityPolicy::default(),
    );

    assert!(matches!(
        preparation,
        ReviewPreparation::Skipped {
            reason: SkipReason::ExplicitOptOut,
            detail: Some(ref detail),
            ..
        } if detail == "privileged_legal_material"
    ));
}

#[test]
fn redacts_unsafe_opt_out_reasons_from_audit_metadata() {
    let preparation = prepare_review(
        PromptReviewInput {
            category: PromptCategory::Review,
            text: "private prompt body",
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: Some("client secret: hunter2"),
        },
        &EligibilityPolicy::default(),
    );

    assert!(matches!(
        preparation,
        ReviewPreparation::Skipped {
            reason: SkipReason::ExplicitOptOut,
            detail: Some(ref detail),
            ..
        } if detail == "redacted_invalid_reason"
    ));
}

#[test]
fn excludes_non_text_and_private_source_inputs() {
    for (content_risk, expected_reason) in [
        (ContentRisk::ContainsNonText, SkipReason::NonText),
        (ContentRisk::PrivateSource, SkipReason::PrivateSource),
    ] {
        let preparation = prepare_review(
            PromptReviewInput {
                category: PromptCategory::SubagentInitial,
                text: "task summary",
                content_risk,
                opt_out_reason: None,
            },
            &EligibilityPolicy::default(),
        );

        assert!(matches!(
            preparation,
            ReviewPreparation::Skipped {
                reason,
                detail: None,
                ..
            } if reason == expected_reason
        ));
    }
}

#[test]
fn excludes_privileged_legal_material_before_redaction() {
    let preparation = prepare_review(
        PromptReviewInput {
            category: PromptCategory::RootUser,
            text: "Attorney-client privileged legal advice follows: do not disclose.",
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: None,
        },
        &EligibilityPolicy::default(),
    );

    assert!(matches!(
        preparation,
        ReviewPreparation::Skipped {
            reason: SkipReason::SensitiveLegalMaterial,
            detail: None,
            ..
        }
    ));
}

#[test]
fn excludes_probable_raw_private_documents() {
    let preparation = prepare_review(
        PromptReviewInput {
            category: PromptCategory::Orchestration,
            text: "Client intake\nName: Jane Doe\nDate of Birth: 2000-01-01\nSocial Security Number: 000-00-0000",
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: None,
        },
        &EligibilityPolicy::default(),
    );

    assert!(matches!(
        preparation,
        ReviewPreparation::Skipped {
            reason: SkipReason::ProbablePrivateDocument,
            detail: None,
            ..
        }
    ));
}

#[test]
fn excludes_single_high_confidence_private_identifier() {
    for text in [
        "Social security number 123-45-6789",
        "MRN: AB-12345",
        "Account number: ZXCV-9876",
        "DOB: 2000-01-02",
    ] {
        assert!(matches!(
            prepare_review(
                PromptReviewInput {
                    category: PromptCategory::RootUser,
                    text,
                    content_risk: ContentRisk::TextOnly,
                    opt_out_reason: None,
                },
                &EligibilityPolicy::default(),
            ),
            ReviewPreparation::Skipped {
                reason: SkipReason::ProbablePrivateDocument,
                ..
            }
        ));
    }
}

#[test]
fn excludes_repository_dumps_and_patches() {
    for text in [
        "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n+secret source",
        "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-private\n+public",
    ] {
        assert!(matches!(
            prepare_review(
                PromptReviewInput {
                    category: PromptCategory::Review,
                    text,
                    content_risk: ContentRisk::TextOnly,
                    opt_out_reason: None,
                },
                &EligibilityPolicy::default(),
            ),
            ReviewPreparation::Skipped {
                reason: SkipReason::RepositoryContent,
                ..
            }
        ));
    }
}

#[test]
fn excludes_binary_text_payloads() {
    let preparation = prepare_review(
        PromptReviewInput {
            category: PromptCategory::Retry,
            text: "retry\0binary",
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: None,
        },
        &EligibilityPolicy::default(),
    );

    assert!(matches!(
        preparation,
        ReviewPreparation::Skipped {
            reason: SkipReason::Binary,
            ..
        }
    ));
}

#[test]
fn excludes_oversized_prompts_without_truncation() {
    let preparation = prepare_review(
        PromptReviewInput {
            category: PromptCategory::SubagentFollowup,
            text: "123456789",
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: None,
        },
        &EligibilityPolicy {
            max_input_bytes: 8,
            ..EligibilityPolicy::default()
        },
    );

    assert!(matches!(
        preparation,
        ReviewPreparation::Skipped {
            reason: SkipReason::Oversized,
            ..
        }
    ));
}

#[test]
fn excludes_private_key_material_instead_of_partially_redacting_it() {
    let preparation = prepare_review(
        PromptReviewInput {
            category: PromptCategory::RootUser,
            text: "-----BEGIN PRIVATE KEY-----\nYWJjZGVm\n-----END PRIVATE KEY-----",
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: None,
        },
        &EligibilityPolicy::default(),
    );

    assert!(matches!(
        preparation,
        ReviewPreparation::Skipped {
            reason: SkipReason::SensitiveCredential,
            ..
        }
    ));
}

#[test]
fn redacts_known_tokens_and_url_credentials() {
    let original =
        "Use ghp_123456789012345678901234567890123456 at https://alice:hunter2@example.com/api";
    let ReviewPreparation::Eligible(prepared) = prepare_review(
        PromptReviewInput {
            category: PromptCategory::RootUser,
            text: original,
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: None,
        },
        &EligibilityPolicy::default(),
    ) else {
        panic!("redactable prompt should be eligible");
    };

    assert_eq!(
        prepared.redacted_text,
        "Use [REDACTED_SECRET] at https://[REDACTED_SECRET]@example.com/api"
    );
}

#[test]
fn redacts_explicit_short_credentials() {
    let original = "password=abc api-key: x Bearer tiny";
    let ReviewPreparation::Eligible(prepared) = prepare_review(
        PromptReviewInput {
            category: PromptCategory::RootUser,
            text: original,
            content_risk: ContentRisk::TextOnly,
            opt_out_reason: None,
        },
        &EligibilityPolicy::default(),
    ) else {
        panic!("redactable prompt should be eligible");
    };

    assert_eq!(
        prepared.redacted_text,
        "password=[REDACTED_SECRET] api-key: [REDACTED_SECRET] Bearer [REDACTED_SECRET]"
    );
}
