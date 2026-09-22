use super::*;
use pretty_assertions::assert_eq;

#[test]
fn parses_strict_allow_response() {
    let response = parse_jev_response(
        br#"{"schema_version":"jev.review.v1","outcome":"allow","advice":null,"reason":null}"#,
    );

    assert_eq!(
        response,
        Ok(JevResponse {
            schema_version: JEV_REVIEW_SCHEMA_VERSION.to_string(),
            outcome: JevOutcome::Allow,
            advice: None,
            reason: None,
        })
    );
}

#[test]
fn rejects_unknown_schema_version() {
    let response = parse_jev_response(
        br#"{"schema_version":"jev.review.v2","outcome":"allow","advice":null,"reason":null}"#,
    );

    assert_eq!(response, Err(JevParseError::UnsupportedVersion));
}

#[test]
fn rejects_advice_outcome_without_advice() {
    let response = parse_jev_response(
        br#"{"schema_version":"jev.review.v1","outcome":"allow_with_advice","advice":null,"reason":null}"#,
    );

    assert_eq!(response, Err(JevParseError::InvalidFields));
}

#[test]
fn rejects_failure_outcomes_without_a_reason() {
    for outcome in ["reject", "unavailable", "malformed"] {
        let response = parse_jev_response(
            format!(
                r#"{{"schema_version":"jev.review.v1","outcome":"{outcome}","advice":null,"reason":null}}"#
            )
            .as_bytes(),
        );

        assert_eq!(response, Err(JevParseError::InvalidFields));
    }
}

#[test]
fn rejects_fields_that_do_not_belong_to_the_selected_outcome() {
    for value in [
        serde_json::json!({
            "schema_version": JEV_REVIEW_SCHEMA_VERSION,
            "outcome": "allow",
            "advice": "unexpected",
            "reason": null,
        }),
        serde_json::json!({
            "schema_version": JEV_REVIEW_SCHEMA_VERSION,
            "outcome": "allow_with_advice",
            "advice": "bounded advice",
            "reason": "unexpected",
        }),
        serde_json::json!({
            "schema_version": JEV_REVIEW_SCHEMA_VERSION,
            "outcome": "reject",
            "advice": "unexpected",
            "reason": "policy",
        }),
    ] {
        assert_eq!(
            parse_jev_response(value.to_string().as_bytes()),
            Err(JevParseError::InvalidFields)
        );
    }
}

#[test]
fn rejects_unknown_fields() {
    let response = parse_jev_response(
        br#"{"schema_version":"jev.review.v1","outcome":"allow","advice":null,"reason":null,"approval":true}"#,
    );

    assert_eq!(response, Err(JevParseError::MalformedJson));
}

#[test]
fn rejects_missing_nullable_fields_and_marks_them_required_in_the_schema() {
    assert_eq!(
        parse_jev_response(
            br#"{"schema_version":"jev.review.v1","outcome":"allow","reason":null}"#,
        ),
        Err(JevParseError::MalformedJson)
    );

    let schema = serde_json::to_value(schemars::schema_for!(JevResponse))
        .expect("serialize Jev response schema");
    let required = schema
        .pointer("/schema/required")
        .or_else(|| schema.pointer("/required"))
        .and_then(serde_json::Value::as_array)
        .expect("required response fields");
    for field in ["schema_version", "outcome", "advice", "reason"] {
        assert!(required.iter().any(|value| value == field), "{field}");
    }
}

#[test]
fn rejects_unbounded_advice_and_reason() {
    for (outcome, advice, reason) in [
        ("allow_with_advice", Some("a".repeat(4_097)), None),
        ("reject", None, Some("r".repeat(1_025))),
    ] {
        let response = parse_jev_response(
            serde_json::json!({
                "schema_version": JEV_REVIEW_SCHEMA_VERSION,
                "outcome": outcome,
                "advice": advice,
                "reason": reason,
            })
            .to_string()
            .as_bytes(),
        );

        assert_eq!(response, Err(JevParseError::InvalidFields));
    }
}

#[test]
fn parses_each_documented_outcome() {
    for (outcome, advice, reason) in [
        ("allow", None, None),
        ("allow_with_advice", Some("keep the scope narrow"), None),
        ("reject", None, Some("policy category")),
        ("unavailable", None, Some("review service offline")),
        ("malformed", None, Some("review response invalid")),
    ] {
        let response = parse_jev_response(
            serde_json::json!({
                "schema_version": JEV_REVIEW_SCHEMA_VERSION,
                "outcome": outcome,
                "advice": advice,
                "reason": reason,
            })
            .to_string()
            .as_bytes(),
        );

        assert!(response.is_ok(), "{outcome} should be valid");
    }
}
