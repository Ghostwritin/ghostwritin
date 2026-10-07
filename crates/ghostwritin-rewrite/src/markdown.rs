//! Splits a draft into what the model may rewrite and what it must not see
//! changed, so the rewrite keeps the Markdown structure.
//!
//! Line-based, deliberately: it recognises the block structure a writer
//! uses, not the full `CommonMark` grammar.
//!
//! **Kept verbatim** (never sent to the model): fenced code blocks, indented
//! code blocks, YAML front matter, headings, thematic breaks, tables, HTML
//! lines, link reference definitions and blank lines. Headings are kept
//! because a rewritten heading breaks links to its anchor.
//!
//! **Rewritten** ([`Piece::Prose`]): paragraphs (consecutive plain lines,
//! one unit), list items and block-quote lines (one unit per line, the
//! marker kept as a prefix). A unit with no letters in it is kept.

/// One piece of a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Reproduced exactly.
    Keep(String),
    /// `prefix` and `suffix` are reproduced exactly; `text` is rewritten.
    Prose {
        prefix: String,
        text: String,
        suffix: String,
    },
}

impl Piece {
    /// Whether this is a paragraph (no list or quote marker), which may
    /// take more lines.
    fn is_paragraph(&self) -> bool {
        matches!(self, Self::Prose { prefix, .. } if prefix.is_empty())
    }
}

/// Splits `text` into pieces whose concatenation (with every `Prose` text
/// unchanged) is `text` again.
pub fn split(text: &str) -> Vec<Piece> {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut front_matter = false;
    for (index, raw) in text.split_inclusive('\n').enumerate() {
        let line = raw.trim_end_matches(['\n', '\r']);
        let newline = &raw[line.len()..];

        if index == 0 && line.trim_end() == "---" {
            front_matter = true;
            keep(&mut pieces, raw);
            continue;
        }
        if front_matter {
            if matches!(line.trim_end(), "---" | "...") {
                front_matter = false;
            }
            keep(&mut pieces, raw);
            continue;
        }
        if let Some((marker, run)) = fence {
            if fence_of(line).is_some_and(|(m, r)| {
                m == marker && r >= run && line.trim().chars().all(|c| c == marker)
            }) {
                fence = None;
            }
            keep(&mut pieces, raw);
            continue;
        }
        if let Some(opened) = fence_of(line) {
            fence = Some(opened);
            keep(&mut pieces, raw);
            continue;
        }

        let continues_paragraph = pieces.last().is_some_and(Piece::is_paragraph);
        let indented = line.starts_with("    ") || line.starts_with('\t');
        if line.trim().is_empty() || is_structural(line) || (indented && !continues_paragraph) {
            keep(&mut pieces, raw);
            continue;
        }

        let marker_len = list_marker(line).or_else(|| quote_marker(line));
        if let Some(len) = marker_len {
            let (prefix, rest) = line.split_at(len);
            prose(&mut pieces, prefix, rest, newline);
        } else if continues_paragraph {
            if let Some(Piece::Prose {
                text: body, suffix, ..
            }) = pieces.last_mut()
            {
                body.push_str(suffix);
                body.push_str(line);
                newline.clone_into(suffix);
            }
        } else {
            prose(&mut pieces, "", line, newline);
        }
    }
    pieces
}

/// Joins pieces back into a document, with `rewritten` texts for the prose
/// pieces in order. A list or quote unit stays on one line.
pub fn join(pieces: &[Piece], rewritten: &[String]) -> String {
    let mut out = String::new();
    let mut next = rewritten.iter();
    for piece in pieces {
        match piece {
            Piece::Keep(text) => out.push_str(text),
            Piece::Prose {
                prefix,
                text,
                suffix,
            } => {
                let body = next.next().map_or(text.as_str(), String::as_str).trim();
                out.push_str(prefix);
                if prefix.is_empty() {
                    out.push_str(body);
                } else {
                    out.push_str(&body.split_whitespace().collect::<Vec<_>>().join(" "));
                }
                out.push_str(suffix);
            }
        }
    }
    out
}

fn keep(pieces: &mut Vec<Piece>, raw: &str) {
    if let Some(Piece::Keep(text)) = pieces.last_mut() {
        text.push_str(raw);
    } else {
        pieces.push(Piece::Keep(raw.to_owned()));
    }
}

fn prose(pieces: &mut Vec<Piece>, prefix: &str, text: &str, newline: &str) {
    if text.chars().any(char::is_alphabetic) {
        pieces.push(Piece::Prose {
            prefix: prefix.to_owned(),
            text: text.to_owned(),
            suffix: newline.to_owned(),
        });
    } else {
        keep(pieces, &format!("{prefix}{text}{newline}"));
    }
}

/// The fence a line opens or closes: three or more backticks or tildes,
/// indented at most three spaces.
fn fence_of(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = trimmed.chars().take_while(|c| *c == marker).count();
    (run >= 3).then_some((marker, run))
}

/// Headings, thematic breaks, tables, HTML and link reference definitions.
fn is_structural(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes)
        && trimmed[hashes..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
    {
        return true;
    }
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() >= 3
        && let Some(first) = compact.chars().next()
        && matches!(first, '-' | '*' | '_')
        && compact.chars().all(|c| c == first)
    {
        return true;
    }
    if trimmed.starts_with('|') || trimmed.starts_with('<') {
        return true;
    }
    // [label]: destination
    trimmed.starts_with('[')
        && trimmed
            .find("]:")
            .is_some_and(|at| at > 1 && !trimmed[1..at].contains(']'))
}

/// The byte length of a list marker and its spacing (`- `, `1. `, `* [ ] `).
fn list_marker(line: &str) -> Option<usize> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    let marker = if rest.starts_with(['-', '*', '+']) {
        1
    } else {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if (1..=9).contains(&digits) && rest[digits..].starts_with(['.', ')']) {
            digits + 1
        } else {
            return None;
        }
    };
    let after = &rest[marker..];
    let space = after.len() - after.trim_start().len();
    if space == 0 || after.trim().is_empty() {
        return None;
    }
    let mut len = indent + marker + space;
    let task = &line[len..];
    for checkbox in ["[ ] ", "[x] ", "[X] "] {
        if task.starts_with(checkbox) {
            len += checkbox.len();
        }
    }
    Some(len)
}

/// The byte length of a block-quote marker run (`> `, `>> `).
fn quote_marker(line: &str) -> Option<usize> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 || !line[indent..].starts_with('>') {
        return None;
    }
    let mut len = indent;
    for c in line[indent..].chars() {
        if c == '>' || c == ' ' {
            len += 1;
        } else {
            break;
        }
    }
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(pieces: &[Piece]) -> Vec<&str> {
        pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Prose { text, .. } => Some(text.as_str()),
                Piece::Keep(_) => None,
            })
            .collect()
    }

    const DOC: &str = "---\ntitle: Draft\n---\n# A heading\n\nFirst paragraph\nwraps here.\n\n- item one\n- [ ] task two\n1. numbered\n\n> quoted line\n\n```rust\nlet x = 1; // not prose\n```\n\n    indented code\n\n| a | b |\n|---|---|\n\n<div>html</div>\n[ref]: https://example.com\n---\nLast line";

    #[test]
    fn prose_units() {
        let pieces = split(DOC);
        assert_eq!(
            texts(&pieces),
            [
                "First paragraph\nwraps here.",
                "item one",
                "task two",
                "numbered",
                "quoted line",
                "Last line"
            ]
        );
    }

    #[test]
    fn split_then_join_is_identity() {
        for doc in [DOC, "", "one line", "a\r\nb\r\n\r\nc\n", "```\nunclosed\n"] {
            let pieces = split(doc);
            let unchanged: Vec<String> = texts(&pieces).iter().map(|t| (*t).to_owned()).collect();
            // Paragraph texts are trimmed on the way back in; these have no
            // surrounding whitespace, so the document comes back as it was.
            assert_eq!(join(&pieces, &unchanged), doc, "{doc:?}");
        }
    }

    #[test]
    fn join_puts_rewrites_in_place() {
        let pieces = split("Para one.\n\n- item\n\n```\ncode\n```\n");
        let out = join(&pieces, &["New one.".to_owned(), "new\nitem".to_owned()]);
        assert_eq!(out, "New one.\n\n- new item\n\n```\ncode\n```\n");
    }

    #[test]
    fn a_unit_without_letters_is_kept() {
        assert!(texts(&split("- 42\n- 2026-10-08\n")).is_empty());
    }

    #[test]
    fn markers() {
        assert_eq!(list_marker("  - x"), Some(4));
        assert_eq!(list_marker("10) x"), Some(4));
        assert_eq!(list_marker("-x"), None);
        assert_eq!(list_marker("1.5 apples"), None);
        assert_eq!(quote_marker(">> deep"), Some(3));
        assert!(is_structural("### Title"));
        assert!(!is_structural("#hashtag start"));
        assert!(is_structural("* * *"));
    }
}
