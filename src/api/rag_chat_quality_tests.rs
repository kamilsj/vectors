use super::*;
use serde_json::json;

const SOURCE: &str = "ATLAS retains daily backups for 21 days. Restores require an administrator.";

fn citations() -> Vec<Citation> {
    ["S1", "S2"]
        .into_iter()
        .map(|label| Citation {
            label: label.into(),
            chunk_id: label.into(),
            document_id: label.into(),
            title: "Guide".into(),
            source: "guide.pdf".into(),
            start_byte: 0,
            end_byte: SOURCE.len(),
        })
        .collect()
}

fn generated(answer: &str) -> GeneratedAnswer {
    GeneratedAnswer {
        answer: answer.into(),
        model: "gpt-4.1-mini".into(),
        input_tokens: 10,
        output_tokens: 20,
        warnings: Vec::new(),
        incomplete: false,
        refused: false,
    }
}

fn checked(value: JsonValue, style: AnswerStyle) -> QualityResult {
    finalize_with_sources(
        &generated(&value.to_string()),
        &citations(),
        &HashMap::from([
            ("S1", SOURCE),
            ("S2", "BETA retains daily backups for 90 days."),
        ]),
        GroundingMode::Strict,
        style,
    )
}

fn supported() -> JsonValue {
    json!({"status":"answered", "answer":"ATLAS keeps daily backups for 21 days. [S1]",
        "evidence":[{"label":"S1","quote":"ATLAS retains daily backups for 21 days."}]})
}

#[test]
fn strict_schema_and_voice_instructions_do_not_change_legacy_body() {
    let original = json!({"instructions":INSTRUCTIONS,"max_output_tokens":2048});
    let mut standard = original.clone();
    augment_body(&mut standard, GroundingMode::Standard, AnswerStyle::Chat);
    assert_eq!(standard, original);
    augment_body(&mut standard, GroundingMode::Strict, AnswerStyle::Voice);
    assert_eq!(standard["text"]["format"]["type"], "json_schema");
    assert_eq!(standard["text"]["format"]["strict"], true);
    assert_eq!(
        standard["text"]["format"]["schema"]["additionalProperties"],
        false
    );
    let instructions = standard["instructions"].as_str().unwrap();
    for required in [
        "untrusted",
        "earlier assistant answers",
        "conflicting evidence",
        "verbatim",
        "voice script",
    ] {
        assert!(instructions.contains(required), "{required}");
    }
}

#[test]
fn accepted_quotes_are_current_and_speech_preserves_facts() {
    let result = checked(supported(), AnswerStyle::Voice);
    assert_eq!(result.answer_status, "answered");
    assert_eq!(result.citation_status, "valid_labels");
    assert_eq!(result.cited_labels, ["S1"]);
    assert_eq!(
        result.evidence[0].quote,
        "ATLAS retains daily backups for 21 days."
    );
    assert_eq!(
        result.speech_text.as_deref(),
        Some("ATLAS keeps daily backups for 21 days.")
    );
    assert!(result.warnings.is_empty());
}

#[test]
fn missing_unknown_malformed_and_stale_labels_withhold_answer_and_speech() {
    for answer in [
        "21 days.",
        "21 days. [S99]",
        "21 days. [S01]",
        "21 days. [S1",
        "21 days. [S1] [Source 2]",
    ] {
        let mut value = supported();
        value["answer"] = answer.into();
        let result = checked(value, AnswerStyle::Voice);
        assert_eq!(result.answer_status, "invalid_grounding", "{answer}");
        assert!(result.answer.is_none());
        assert!(result.speech_text.is_none());
        assert!(result.evidence.is_empty());
    }
}

#[test]
fn quotations_from_other_documents_or_older_history_fail() {
    for quote in [
        "ATLAS retains daily backups for 99 days.",
        "BETA retains daily backups for 90 days.",
        "21",
        "   ",
        "ATLAS  retains daily backups for 21 days.",
    ] {
        let mut value = supported();
        value["evidence"][0]["quote"] = quote.into();
        assert_eq!(
            checked(value, AnswerStyle::Voice).answer_status,
            "invalid_grounding",
            "{quote}"
        );
    }
}

#[test]
fn every_citation_needs_its_own_matching_evidence() {
    let mut value = supported();
    value["answer"] = "ATLAS keeps backups for 21 days; BETA for 90 days. [S1] [S2]".into();
    assert_eq!(
        checked(value.clone(), AnswerStyle::Chat).answer_status,
        "invalid_grounding"
    );
    value["evidence"]
        .as_array_mut()
        .unwrap()
        .push(json!({"label":"S2","quote":"BETA retains daily backups for 90 days."}));
    let result = checked(value.clone(), AnswerStyle::Chat);
    assert_eq!(result.answer_status, "answered");
    assert_eq!(result.cited_labels, ["S1", "S2"]);
    assert!(result.speech_text.is_none());
    value["answer"] = "ATLAS keeps daily backups for 21 days. [S1]".into();
    assert_eq!(
        checked(value, AnswerStyle::Chat).answer_status,
        "invalid_grounding"
    );
}

#[test]
fn abstention_uses_safe_text_and_cannot_hide_factual_claims() {
    let value = json!({"status":"insufficient_evidence","answer":"Actually the retention is 500 days.","evidence":[]});
    let result = checked(value.clone(), AnswerStyle::Voice);
    assert_eq!(result.answer_status, "insufficient_evidence");
    assert_eq!(result.answer.as_deref(), Some(INSUFFICIENT_ANSWER));
    assert_eq!(result.speech_text.as_deref(), Some(INSUFFICIENT_ANSWER));
    assert_eq!(result.citation_status, "not_applicable");
    let mut cited = value;
    cited["answer"] = "500 days. [S1]".into();
    assert_eq!(
        checked(cited, AnswerStyle::Voice).answer_status,
        "invalid_grounding"
    );
}

#[test]
fn empty_and_whitespace_abstentions_use_safe_fallback_for_chat_and_voice() {
    for style in [AnswerStyle::Chat, AnswerStyle::Voice] {
        for answer in ["", " \t\r\n"] {
            let result = checked(
                json!({"status":"insufficient_evidence","answer":answer,"evidence":[]}),
                style,
            );
            assert_eq!(result.answer_status, "insufficient_evidence");
            assert_eq!(result.answer.as_deref(), Some(INSUFFICIENT_ANSWER));
            assert_eq!(
                result.speech_text.as_deref(),
                (style == AnswerStyle::Voice).then_some(INSUFFICIENT_ANSWER)
            );
            assert_eq!(result.citation_status, "not_applicable");
            assert!(result.cited_labels.is_empty());
            assert!(result.evidence.is_empty());
            assert!(result.warnings.is_empty());
        }
    }
}

#[test]
fn empty_answers_and_clarifications_remain_invalid() {
    for style in [AnswerStyle::Chat, AnswerStyle::Voice] {
        for status in ["answered", "clarification_needed"] {
            for answer in ["", " \t\r\n"] {
                let result = checked(
                    json!({"status":status,"answer":answer,"evidence":[]}),
                    style,
                );
                assert_eq!(result.answer_status, "invalid_grounding");
                assert!(result.answer.is_none());
                assert!(result.speech_text.is_none());
            }
        }
    }
}

#[test]
fn abstention_still_rejects_invalid_text_and_nonempty_evidence() {
    for value in [
        json!({"status":"insufficient_evidence","answer":"\0","evidence":[]}),
        json!({"status":"insufficient_evidence","answer":"x".repeat(MAX_ANSWER_BYTES + 1),"evidence":[]}),
        json!({"status":"insufficient_evidence","answer":"","evidence":[{"label":"S1","quote":SOURCE}]}),
    ] {
        let result = checked(value, AnswerStyle::Voice);
        assert_eq!(result.answer_status, "invalid_grounding");
        assert!(result.answer.is_none());
        assert!(result.speech_text.is_none());
        assert!(result.evidence.is_empty());
    }
}

#[test]
fn clarification_requires_a_question_without_source_claims() {
    let mut value = json!({"status":"clarification_needed","answer":"Do you mean ATLAS or BETA?","evidence":[]});
    let result = checked(value.clone(), AnswerStyle::Voice);
    assert_eq!(result.answer_status, "clarification_needed");
    assert_eq!(
        result.speech_text.as_deref(),
        Some("Do you mean ATLAS or BETA?")
    );
    value["answer"] = "You mean ATLAS.".into();
    assert_eq!(
        checked(value, AnswerStyle::Voice).answer_status,
        "invalid_grounding"
    );
}

#[test]
fn partial_and_refused_output_never_becomes_speech() {
    for refused in [false, true] {
        let mut output = generated(&supported().to_string());
        output.refused = refused;
        output.incomplete = !refused;
        for grounding in [GroundingMode::Strict, GroundingMode::Standard] {
            let result = finalize_with_sources(
                &output,
                &citations(),
                &HashMap::from([("S1", SOURCE)]),
                grounding,
                AnswerStyle::Voice,
            );
            assert_eq!(
                result.answer_status,
                if refused { "refused" } else { "incomplete" }
            );
            assert!(result.speech_text.is_none());
            assert_eq!(
                result.answer.is_some(),
                grounding == GroundingMode::Standard
            );
        }
    }
}

#[test]
fn invalid_json_unknown_fields_empty_and_excess_evidence_fail_closed() {
    for value in [
        json!("raw answer"),
        json!({"status":"answered","answer":"Yes [S1]"}),
        json!({"status":"unknown","answer":"Yes","evidence":[]}),
        json!({"status":"answered","answer":"","evidence":[]}),
    ] {
        assert_eq!(
            checked(value, AnswerStyle::Voice).answer_status,
            "invalid_grounding"
        );
    }
    let mut extra = supported();
    extra["confidence"] = 1.into();
    assert_eq!(
        checked(extra, AnswerStyle::Chat).answer_status,
        "invalid_grounding"
    );
    let mut excess = supported();
    excess["evidence"] = json!(vec![json!({"label":"S1","quote":SOURCE}); 101]);
    assert_eq!(
        checked(excess, AnswerStyle::Chat).answer_status,
        "invalid_grounding"
    );
}

#[test]
fn tiny_source_needs_complete_substantive_quote() {
    let sources = HashMap::from([("S1", "Yes.")]);
    for (quote, status) in [
        ("Yes.", "answered"),
        ("Yes", "invalid_grounding"),
        ("", "invalid_grounding"),
    ] {
        let value = json!({"status":"answered","answer":"Yes. [S1]","evidence":[{"label":"S1","quote":quote}]});
        let result = finalize_with_sources(
            &generated(&value.to_string()),
            &citations(),
            &sources,
            GroundingMode::Strict,
            AnswerStyle::Chat,
        );
        assert_eq!(result.answer_status, status);
    }
}

#[test]
fn standard_answers_remain_compatible_but_invalid_labels_block_speech() {
    for (answer, status) in [
        ("Twenty-one days.", "missing"),
        ("Twenty-one days. [S99]", "invalid"),
    ] {
        let result = finalize_with_sources(
            &generated(answer),
            &citations(),
            &HashMap::new(),
            GroundingMode::Standard,
            AnswerStyle::Voice,
        );
        assert_eq!(result.answer.as_deref(), Some(answer));
        assert_eq!(result.citation_status, status);
        assert!(result.speech_text.is_none());
        assert!(!result.warnings.is_empty());
    }
}

#[test]
fn speech_does_not_strip_meaningful_punctuation_or_paraphrase() {
    let mut value = supported();
    value["answer"] = "ATLAS keeps backups for 21 days (not indefinitely). [S1]\nAn administrator must restore them. [S1]".into();
    let result = checked(value, AnswerStyle::Voice);
    assert_eq!(result.speech_text.as_deref(), Some("ATLAS keeps backups for 21 days (not indefinitely). An administrator must restore them."));
    assert_eq!(result.cited_labels, ["S1"]);
}

#[test]
fn non_plain_or_long_answers_do_not_become_voice_scripts() {
    for answer in [
        "**21 days** [S1]".to_owned(),
        "- 21 days [S1]".to_owned(),
        "1. 21 days [S1]".to_owned(),
        format!("{} [S1]", "word ".repeat(121)),
    ] {
        let mut value = supported();
        value["answer"] = answer.into();
        let result = checked(value, AnswerStyle::Voice);
        assert_eq!(result.answer_status, "answered");
        assert!(result.speech_text.is_none());
        assert!(!result.warnings.is_empty());
    }
}

#[test]
fn source_quote_matching_is_provenance_not_factual_entailment() {
    // A separate semantic or human evaluation is needed to reject this false
    // claim: a valid quotation alone cannot establish that it entails an answer.
    let mut value = supported();
    value["answer"] = "ATLAS keeps backups for 99 days. [S1]".into();
    assert_eq!(checked(value, AnswerStyle::Chat).answer_status, "answered");
}
