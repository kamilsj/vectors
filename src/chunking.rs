//! Deterministic document chunks with exact UTF-8 source offsets.
//!
//! Lengths count Unicode scalar values, not bytes, grapheme clusters, or model
//! tokens. Natural boundaries are preferred when a section needs splitting;
//! Markdown headings begin a new section and remain separate context metadata.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Maximum UTF-8 document size accepted before chunking allocates working data.
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
/// Maximum Unicode scalar values in one chunk (at most 32,000 UTF-8 bytes).
pub const MAX_CHUNK_CHARACTERS: usize = 8_000;
/// Hard upper bound on the number of chunks produced by one document.
pub const MAX_DOCUMENT_CHUNKS: usize = 256;
/// Heading context is bounded so a chunk plus its heading fits in 32 KiB.
pub const MAX_HEADING_CHARACTERS: usize = 160;

/// Character-based document splitting settings.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ChunkingConfig {
    /// Maximum Unicode scalar values in each chunk, from 1 through 8,000.
    pub max_characters: usize,
    /// Requested maximum overlap. Effective overlap never exceeds half the
    /// previous chunk, and does not cross a Markdown heading boundary.
    pub overlap_characters: usize,
    /// Maximum accepted chunk count, from 1 through 256. Exceeding the limit
    /// rejects the entire document instead of returning a partial result.
    pub max_chunks: usize,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            max_characters: 1_200,
            overlap_characters: 150,
            max_chunks: MAX_DOCUMENT_CHUNKS,
        }
    }
}

/// A verbatim source range and its optional Markdown heading context.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TextChunk {
    /// Zero-based position in the returned document chunks.
    pub ordinal: usize,
    /// Exactly `source[byte_start..byte_end]`, without any prepended context.
    pub text: String,
    /// Inclusive byte offset on a UTF-8 character boundary.
    pub byte_start: usize,
    /// Exclusive byte offset on a UTF-8 character boundary.
    pub byte_end: usize,
    /// Most recent heading at the chunk's start, without Markdown markers,
    /// truncated to at most 160 Unicode scalar values.
    pub heading: Option<String>,
}

/// Split text while preserving every source character and its byte offsets.
///
/// Paragraph boundaries take precedence over sentence endings, then whitespace
/// and finally a character boundary. Short natural fragments are avoided unless
/// a Markdown heading starts a new section. Overlap is reduced at word boundaries
/// where possible, is at most half the preceding chunk, and never crosses into
/// another heading section. Every successive chunk advances the covered range.
///
/// ATX (`# Heading`) and single-line Setext headings are recognized outside
/// fenced code blocks. Empty text produces no chunks; whitespace is preserved.
/// Invalid settings, documents larger than 1 MiB, and results requiring too many
/// chunks return an error before any chunks are exposed to the caller.
pub fn chunk_text(text: &str, config: &ChunkingConfig) -> Result<Vec<TextChunk>> {
    validate_config(config)?;
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::InvalidQuery(format!(
            "document exceeds the {MAX_DOCUMENT_BYTES}-byte chunking limit"
        )));
    }
    if text.is_empty() {
        return Ok(Vec::new());
    }

    let characters = text.chars().collect::<Vec<_>>();
    // This is a lower bound even with zero overlap and no natural breaks.
    if characters.len() > config.max_characters * config.max_chunks {
        return Err(too_many_chunks(config.max_chunks));
    }
    let offsets = text
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let boundaries = natural_boundaries(&characters);
    let headings = markdown_headings(text);
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut covered_end = 0;

    while start < characters.len() {
        if chunks.len() == config.max_chunks {
            return Err(too_many_chunks(config.max_chunks));
        }
        let next_heading = headings.partition_point(|heading| heading.byte_start <= offsets[start]);
        let section_end = headings
            .get(next_heading)
            .map_or(characters.len(), |heading| {
                offsets
                    .binary_search(&heading.byte_start)
                    .expect("heading starts at a UTF-8 line boundary")
            });
        let hard_end = (start + config.max_characters).min(section_end);
        let end = if hard_end == section_end {
            hard_end
        } else {
            let minimum = (start + (config.max_characters / 2).max(1)).max(covered_end + 1);
            preferred_end(&boundaries, minimum, hard_end)
        };
        debug_assert!(end > covered_end && end > start);
        let heading = next_heading
            .checked_sub(1)
            .map(|index| &headings[index].title)
            .filter(|title| !title.is_empty())
            .cloned();
        chunks.push(TextChunk {
            ordinal: chunks.len(),
            text: text[offsets[start]..offsets[end]].to_owned(),
            byte_start: offsets[start],
            byte_end: offsets[end],
            heading,
        });
        covered_end = end;
        if end == characters.len() {
            break;
        }
        if end == section_end {
            // Context from one section must not be attached to a neighboring
            // section merely because an overlap copied its heading backward.
            start = end;
            continue;
        }
        let overlap = config.overlap_characters.min((end - start) / 2);
        let earliest = end - overlap;
        start = boundaries[earliest..end]
            .iter()
            .position(|boundary| *boundary > 0)
            .map_or(earliest, |relative| earliest + relative);
    }
    Ok(chunks)
}

fn validate_config(config: &ChunkingConfig) -> Result<()> {
    if !(1..=MAX_CHUNK_CHARACTERS).contains(&config.max_characters) {
        return Err(Error::InvalidQuery(format!(
            "max_characters must be between 1 and {MAX_CHUNK_CHARACTERS}"
        )));
    }
    if config.overlap_characters >= config.max_characters {
        return Err(Error::InvalidQuery(
            "overlap_characters must be smaller than max_characters".into(),
        ));
    }
    if !(1..=MAX_DOCUMENT_CHUNKS).contains(&config.max_chunks) {
        return Err(Error::InvalidQuery(format!(
            "max_chunks must be between 1 and {MAX_DOCUMENT_CHUNKS}"
        )));
    }
    Ok(())
}

fn too_many_chunks(maximum: usize) -> Error {
    Error::InvalidQuery(format!(
        "document requires more than max_chunks ({maximum}); increase chunk size or split the document"
    ))
}

fn preferred_end(boundaries: &[u8], minimum: usize, maximum: usize) -> usize {
    for preference in (1..=3).rev() {
        if let Some(relative) = boundaries[minimum..=maximum]
            .iter()
            .rposition(|boundary| *boundary == preference)
        {
            return minimum + relative;
        }
    }
    maximum
}

fn natural_boundaries(characters: &[char]) -> Vec<u8> {
    let mut boundaries = vec![0; characters.len() + 1];
    let mut line_has_content = false;
    for (index, character) in characters.iter().copied().enumerate() {
        if character == '\n' {
            if !line_has_content {
                boundaries[index + 1] = 3;
            }
            line_has_content = false;
        } else if !character.is_whitespace() {
            line_has_content = true;
        }
        if character.is_whitespace()
            && characters
                .get(index + 1)
                .is_none_or(|next| !next.is_whitespace())
        {
            boundaries[index + 1] = boundaries[index + 1].max(1);
        }
        if !matches!(character, '.' | '!' | '?' | '。' | '！' | '？') {
            continue;
        }
        let mut end = index + 1;
        while characters.get(end).is_some_and(|next| {
            matches!(
                next,
                '"' | '\'' | '’' | '”' | ')' | ']' | '}' | '»' | '」' | '』'
            )
        }) {
            end += 1;
        }
        if matches!(character, '。' | '！' | '？')
            || characters.get(end).is_none_or(|next| next.is_whitespace())
        {
            while characters.get(end).is_some_and(|next| next.is_whitespace()) {
                end += 1;
            }
            boundaries[end] = boundaries[end].max(2);
        }
    }
    boundaries
}

struct Heading {
    byte_start: usize,
    title: String,
}

fn markdown_headings(text: &str) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut byte_start = 0;
    let mut fenced: Option<(char, usize)> = None;
    let mut previous: Option<(usize, &str)> = None;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let indentation = content.bytes().take_while(|byte| *byte == b' ').count();
        let body = &content[indentation..];
        if indentation > 3 || body.starts_with('\t') {
            previous = None;
            byte_start += line.len();
            continue;
        }
        if let Some((marker, length)) = fence_marker(body) {
            match fenced {
                Some((opening, minimum))
                    if marker == opening
                        && length >= minimum
                        && body[length..].trim().is_empty() =>
                {
                    fenced = None;
                }
                None => fenced = Some((marker, length)),
                _ => {}
            }
            previous = None;
            byte_start += line.len();
            continue;
        }
        if fenced.is_some() {
            previous = None;
            byte_start += line.len();
            continue;
        }
        if let Some(title) = atx_title(body) {
            headings.push(Heading {
                byte_start,
                title: bounded_title(title),
            });
            previous = None;
        } else if is_setext_underline(body) {
            if let Some((title_start, title)) = previous.take() {
                headings.push(Heading {
                    byte_start: title_start,
                    title: bounded_title(title),
                });
            }
        } else {
            previous = (!body.trim().is_empty()).then_some((byte_start, body.trim()));
        }
        byte_start += line.len();
    }
    headings
}

fn fence_marker(line: &str) -> Option<(char, usize)> {
    let marker = line.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let length = line
        .chars()
        .take_while(|character| *character == marker)
        .count();
    (length >= 3).then_some((marker, length))
}

fn atx_title(line: &str) -> Option<&str> {
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&level)
        || line[level..]
            .chars()
            .next()
            .is_some_and(|character| !matches!(character, ' ' | '\t'))
    {
        return None;
    }
    let title = line[level..].trim();
    let without_closing = title.trim_end_matches('#');
    if without_closing.is_empty() || without_closing.ends_with([' ', '\t']) {
        Some(without_closing.trim_end())
    } else {
        Some(title)
    }
}

fn is_setext_underline(line: &str) -> bool {
    let line = line.trim_end();
    !line.is_empty()
        && (line.bytes().all(|byte| byte == b'=') || line.bytes().all(|byte| byte == b'-'))
}

fn bounded_title(title: &str) -> String {
    title.chars().take(MAX_HEADING_CHARACTERS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(max_characters: usize, overlap_characters: usize) -> ChunkingConfig {
        ChunkingConfig {
            max_characters,
            overlap_characters,
            max_chunks: MAX_DOCUMENT_CHUNKS,
        }
    }

    fn assert_ranges(source: &str, settings: &ChunkingConfig, chunks: &[TextChunk]) {
        if source.is_empty() {
            assert!(chunks.is_empty());
            return;
        }
        assert!(!chunks.is_empty());
        assert!(chunks.len() <= settings.max_chunks);
        assert_eq!(chunks[0].byte_start, 0);
        let mut covered_end = 0;
        let mut previous_start = None;
        for (ordinal, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.ordinal, ordinal);
            assert!(source.is_char_boundary(chunk.byte_start));
            assert!(source.is_char_boundary(chunk.byte_end));
            assert_eq!(chunk.text, source[chunk.byte_start..chunk.byte_end]);
            assert!(!chunk.text.is_empty());
            assert!(chunk.text.chars().count() <= settings.max_characters);
            assert!(chunk.byte_start <= covered_end, "gap in source coverage");
            assert!(
                chunk.byte_end > covered_end,
                "chunk made no forward progress"
            );
            if let Some(previous_start) = previous_start {
                assert!(chunk.byte_start > previous_start);
                let overlap = source[chunk.byte_start..covered_end].chars().count();
                assert!(overlap <= settings.overlap_characters);
            }
            covered_end = chunk.byte_end;
            previous_start = Some(chunk.byte_start);
        }
        assert_eq!(covered_end, source.len());
    }

    #[test]
    fn settings_have_explicit_defaults_and_reject_unknown_fields() {
        let defaults: ChunkingConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults, ChunkingConfig::default());
        assert_eq!(defaults.max_characters, 1_200);
        assert_eq!(defaults.overlap_characters, 150);
        assert_eq!(defaults.max_chunks, 256);
        let partial: ChunkingConfig = serde_json::from_str(r#"{"overlap_characters":20}"#).unwrap();
        assert_eq!(partial.overlap_characters, 20);
        assert_eq!(partial.max_characters, 1_200);
        assert!(serde_json::from_str::<ChunkingConfig>(r#"{"max_tokens":512}"#).is_err());
    }

    #[test]
    fn unicode_offsets_reconstruct_verbatim_source_with_overlap() {
        let text = "Zażółć 🧑‍💻 gęślą jaźń. e\u{301}lan 中文。\r\n\r\n次の段落です！\n".repeat(7);
        let settings = config(23, 7);
        let chunks = chunk_text(&text, &settings).unwrap();
        assert_ranges(&text, &settings, &chunks);
        assert_eq!(chunks, chunk_text(&text, &settings).unwrap());
        assert!(chunks
            .windows(2)
            .any(|pair| pair[1].byte_start < pair[0].byte_end));
    }

    #[test]
    fn paragraphs_take_precedence_over_later_sentence_and_word_breaks() {
        let paragraph = "Paragraph one has enough words.\n\n";
        let text =
            format!("{paragraph}Sentence two. More words to exceed the maximum chunk length.");
        let settings = config(64, 0);
        let chunks = chunk_text(&text, &settings).unwrap();
        assert_eq!(chunks[0].text, paragraph);
        assert_ranges(&text, &settings, &chunks);
    }

    #[test]
    fn sentence_boundaries_include_closing_quotes_and_trailing_whitespace() {
        let first = "The first sentence has enough words.\" ";
        let text = format!("{first}The second sentence is even longer and requires splitting.");
        let settings = config(64, 0);
        let chunks = chunk_text(&text, &settings).unwrap();
        assert_eq!(chunks[0].text, first);
        assert_ranges(&text, &settings, &chunks);
    }

    #[test]
    fn whitespace_and_long_word_fallbacks_preserve_text() {
        let text = "alpha beta gamma delta epsilon zeta";
        let settings = config(13, 0);
        let chunks = chunk_text(text, &settings).unwrap();
        assert_eq!(chunks[0].text, "alpha beta ");
        assert_ranges(text, &settings, &chunks);
        let long_word = "🦀".repeat(43);
        let settings = config(7, 3);
        let chunks = chunk_text(&long_word, &settings).unwrap();
        assert_ranges(&long_word, &settings, &chunks);
        assert_eq!(chunks[0].text.chars().count(), 7);
    }

    #[test]
    fn large_requested_overlap_is_bounded_and_always_adds_new_content() {
        let text = "x".repeat(100);
        let settings = config(10, 9);
        let chunks = chunk_text(&text, &settings).unwrap();
        assert_eq!(chunks.len(), 19);
        assert_ranges(&text, &settings, &chunks);
        for pair in chunks.windows(2) {
            assert!(pair[0].byte_end - pair[1].byte_start <= pair[0].text.len() / 2);
        }
    }

    #[test]
    fn heading_transitions_keep_context_and_overlap_in_the_correct_section() {
        let text = "Introduction.\n# First section\nSome long text about the first subject. More words follow here.\n\n## Second section ##\nA different subject now has its own heading. More words again.\n";
        let first = text.find("# First").unwrap();
        let second = text.find("## Second").unwrap();
        let settings = config(46, 12);
        let chunks = chunk_text(text, &settings).unwrap();
        assert_ranges(text, &settings, &chunks);
        for chunk in &chunks {
            let expected = if chunk.byte_start < first {
                None
            } else if chunk.byte_start < second {
                Some("First section")
            } else {
                Some("Second section")
            };
            assert_eq!(chunk.heading.as_deref(), expected);
            assert!(chunk.byte_end <= first || chunk.byte_start >= first);
            assert!(chunk.byte_end <= second || chunk.byte_start >= second);
        }
        assert!(chunks.iter().any(|chunk| chunk.byte_start == first));
        assert!(chunks.iter().any(|chunk| chunk.byte_start == second));
    }

    #[test]
    fn fenced_code_does_not_create_headings_and_setext_headings_are_supported() {
        let text = "# Real\r\nText before code.\r\n```markdown\r\n# Not a heading\r\nAlso not a heading\r\n===\r\n```\r\nText after code.\r\n\r\nOther section\r\n-------------\r\nMore text.\r\n~~~\r\n## Still code\r\n~~~\r\nDone.";
        let other = text.find("Other section").unwrap();
        let settings = config(37, 8);
        let chunks = chunk_text(text, &settings).unwrap();
        assert_ranges(text, &settings, &chunks);
        for chunk in &chunks {
            assert_eq!(
                chunk.heading.as_deref(),
                Some(if chunk.byte_start < other {
                    "Real"
                } else {
                    "Other section"
                })
            );
        }
        assert!(chunks.iter().any(|chunk| chunk.byte_start == other));
    }

    #[test]
    fn heading_context_and_maximum_chunks_fit_embedding_text_byte_limit() {
        let text = format!("# {}\n{}", "🌍".repeat(300), "🦀".repeat(10_000));
        let settings = config(MAX_CHUNK_CHARACTERS, 0);
        let chunks = chunk_text(&text, &settings).unwrap();
        assert_ranges(&text, &settings, &chunks);
        for chunk in chunks {
            let heading = chunk.heading.unwrap();
            assert_eq!(heading.chars().count(), MAX_HEADING_CHARACTERS);
            assert!(heading.len() + 2 + chunk.text.len() <= 32 * 1024);
        }
    }

    #[test]
    fn empty_headings_reset_context_and_hashes_in_words_are_preserved() {
        let text = "# Original\nFirst text.\n## ###\nNo title context here.\n### C#\nLast text.";
        let settings = config(100, 10);
        let chunks = chunk_text(text, &settings).unwrap();
        assert_ranges(text, &settings, &chunks);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].heading.as_deref(), Some("Original"));
        assert_eq!(chunks[1].heading, None);
        assert_eq!(chunks[2].heading.as_deref(), Some("C#"));
    }

    #[test]
    fn invalid_limits_and_oversized_documents_fail_before_returning_chunks() {
        for settings in [
            config(0, 0),
            config(MAX_CHUNK_CHARACTERS + 1, 0),
            config(10, 10),
            ChunkingConfig {
                max_chunks: 0,
                ..config(10, 0)
            },
            ChunkingConfig {
                max_chunks: MAX_DOCUMENT_CHUNKS + 1,
                ..config(10, 0)
            },
        ] {
            assert!(matches!(
                chunk_text("", &settings),
                Err(Error::InvalidQuery(_))
            ));
        }
        let oversized = "🦀".repeat(MAX_DOCUMENT_BYTES / 4 + 1);
        assert!(matches!(
            chunk_text(&oversized, &ChunkingConfig::default()),
            Err(Error::InvalidQuery(_))
        ));
        let settings = ChunkingConfig {
            max_chunks: 2,
            ..config(10, 0)
        };
        assert!(chunk_text(&"x".repeat(21), &settings)
            .unwrap_err()
            .to_string()
            .contains("max_chunks"));
        let settings = ChunkingConfig {
            max_chunks: 2,
            ..config(100, 0)
        };
        assert!(chunk_text("# A\nx\n# B\ny\n# C\nz", &settings)
            .unwrap_err()
            .to_string()
            .contains("max_chunks"));
    }

    #[test]
    fn empty_and_whitespace_documents_do_not_produce_empty_chunks_or_drop_text() {
        let settings = config(8, 3);
        assert!(chunk_text("", &settings).unwrap().is_empty());
        let text = " \r\n\t\n".repeat(9);
        assert_ranges(&text, &settings, &chunk_text(&text, &settings).unwrap());
    }

    #[test]
    fn small_limits_and_dense_boundaries_preserve_coverage_and_progress() {
        let texts = [
            "a b. c! d? e\n\nf g\n\nh i j\n",
            "一句話。二句話！第三句？下一句。",
            "\n\n \n\n \n",
            "e\u{301}🧑‍💻abcdefg",
        ];
        for maximum in 1..=17 {
            for overlap in [0, maximum / 2, maximum - 1] {
                let settings = config(maximum, overlap);
                for text in texts {
                    let chunks = chunk_text(text, &settings).unwrap();
                    assert_ranges(text, &settings, &chunks);
                }
            }
        }
    }

    #[test]
    fn generated_unicode_documents_keep_range_invariants_across_boundary_settings() {
        let alphabet = [
            'a', ' ', '\n', '\r', '🙂', 'é', '\u{301}', '。', '.', ')', '#', '`', '~',
        ];
        for seed in 0..32_u64 {
            let mut state = seed;
            let mut text = String::from("# A\n");
            for _ in 0..48 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                text.push(alphabet[(state >> 32) as usize % alphabet.len()]);
            }
            text.push_str("\n\n## B\n");
            for _ in 0..48 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                text.push(alphabet[(state >> 32) as usize % alphabet.len()]);
            }
            for maximum in 1..=24 {
                for overlap in [0, maximum / 2, maximum - 1] {
                    let settings = config(maximum, overlap);
                    let chunks = chunk_text(&text, &settings).unwrap();
                    assert_ranges(&text, &settings, &chunks);
                }
            }
        }
    }
}
