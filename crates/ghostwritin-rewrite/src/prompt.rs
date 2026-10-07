//! The prompts: Ghostwritin's own, so they live here, not in the harness.

use std::fmt::Write as _;

use cratefield_text_guard::{Guard, SpanKind, Violation, ViolationKind};
use ghostwritin_core::{Strength, StyleSummary, Voice};

fn voice_guidance(voice: Voice, style: Option<&StyleSummary>) -> String {
    match voice {
        Voice::Casual => "Casual: plain words, contractions, short sentences, the way a \
                          thoughtful person writes to a friend. No slang the writer did not use."
            .to_owned(),
        Voice::Professional => "Professional: clear and direct, as to a client or a colleague. \
                                Concrete verbs, no filler, no hype."
            .to_owned(),
        Voice::Academic => "Academic: precise and formal, claims hedged exactly as much as \
                            the evidence in the draft supports, no rhetorical flourish."
            .to_owned(),
        Voice::MyVoice => {
            let mut text = "The writer's own voice, described by this style summary built \
                            from their own writing:\n"
                .to_owned();
            text.push_str(style.map_or("(none)", StyleSummary::as_str));
            text
        }
    }
}

fn strength_guidance(strength: Strength) -> &'static str {
    match strength {
        Strength::Polish => {
            "Polish: keep each sentence and its order. Change only the words and phrases \
             that read as machine-written (stock transitions, inflated vocabulary, empty \
             intensifiers, formulaic summaries)."
        }
        Strength::Edit => {
            "Edit: rework sentences freely, merge or split them, but keep each paragraph's \
             points in the same order."
        }
        Strength::Rewrite => {
            "Rewrite: write each paragraph again from scratch in the voice, from the same \
             facts and the same argument. Cut padding entirely."
        }
    }
}

/// The system prompt for a voice and strength.
pub(crate) fn system(voice: Voice, strength: Strength, style: Option<&StyleSummary>) -> String {
    format!(
        "You are Ghostwritin', an editor who rewrites drafts into a person's natural voice \
         without changing what they say.\n\n\
         Voice. {voice}\n\n\
         Strength. {strength}\n\n\
         Rules, all of them strict:\n\
         1. Keep the meaning. Add no facts, claims, examples or opinions; drop none.\n\
         2. Every locked fact listed for a paragraph must appear in your rewrite of that \
            paragraph exactly as written, character for character: names, numbers (with \
            their currency and percent signs), quoted words and code. You may move them. \
            Write no number that is not in the paragraph.\n\
         3. Inline code (between backticks) stays byte for byte, backticks included.\n\
         4. Keep Markdown inline formatting (links, emphasis) where the words it marks \
            survive.\n\
         5. Write in the draft's language.\n\
         6. Answer with JSON only: {{\"paragraphs\": [...]}}, one rewritten string per \
            input paragraph, in the same order, the same count.",
        voice = voice_guidance(voice, style),
        strength = strength_guidance(strength),
    )
}

/// The user turn: the paragraphs as JSON, and each one's locked facts.
pub(crate) fn task(paragraphs: &[&str]) -> String {
    let guard = Guard::new();
    let mut text = format!(
        "Rewrite these {} paragraph(s).\n\nLocked facts per paragraph (keep verbatim):\n",
        paragraphs.len()
    );
    for (index, paragraph) in paragraphs.iter().enumerate() {
        let locks: Vec<String> = guard
            .extract(paragraph)
            .iter()
            .map(|span| {
                serde_json::to_string(span.protected()).unwrap_or_else(|_| String::from("\"\""))
            })
            .collect();
        let _ = writeln!(
            text,
            "{}: {}",
            index + 1,
            if locks.is_empty() {
                "none".to_owned()
            } else {
                locks.join(", ")
            }
        );
    }
    let input = serde_json::json!({ "paragraphs": paragraphs });
    let _ = write!(text, "\nInput:\n{input}");
    text
}

/// The repair turn after a rewrite broke locks: which paragraph, which
/// facts. Sent to the model only, never logged.
pub(crate) fn repair(
    failures: &[(usize, Vec<Violation>)],
    count_mismatch: Option<usize>,
) -> String {
    let mut text = String::from(
        "Your rewrite broke the rules. Fix exactly this and answer again with the full JSON:\n",
    );
    if let Some(expected) = count_mismatch {
        let _ = writeln!(
            text,
            "- Return exactly {expected} paragraphs, one per input paragraph, in order."
        );
    }
    for (index, violations) in failures {
        for violation in violations {
            let quoted = serde_json::to_string(violation.span.protected())
                .unwrap_or_else(|_| String::from("\"\""));
            let what = match (violation.kind, violation.span.kind) {
                (ViolationKind::Missing, SpanKind::Quote) => {
                    format!("the quoted words {quoted} must appear verbatim")
                }
                (ViolationKind::Missing, _) => format!("{quoted} must appear exactly as written"),
                (ViolationKind::Introduced, _) => {
                    format!("{quoted} is not in the original; remove it")
                }
            };
            let _ = writeln!(text, "- Paragraph {}: {what}.", index + 1);
        }
    }
    text
}
