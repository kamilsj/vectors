use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use vectors::{
    ComputeConfig, ComputeDevice, Database, GraphChunkInput, GraphCollectionConfig,
    GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest, GraphRagRequest,
};

fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: 3,
        context_format_version: 1,
    }
}

fn database(texts: &[String]) -> Database {
    let database = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    database
        .graph_create_collection(GraphCollectionConfig {
            name: "lexical".into(),
            profile: profile(),
            semantic_neighbors: 0,
            semantic_threshold: 0.8,
        })
        .unwrap();
    let mut source = String::new();
    let chunks = texts
        .iter()
        .map(|text| {
            let start_byte = source.len();
            source.push_str(text);
            GraphChunkInput {
                start_byte,
                end_byte: source.len(),
                text: text.clone(),
                embedding_text: text.clone(),
                embedding: vectors::Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
            }
        })
        .collect();
    database
        .graph_ingest_document(GraphIngestRequest {
            collection: "lexical".into(),
            expected_revision: database.revision().unwrap(),
            expected_profile: profile(),
            document: GraphDocumentInput {
                id: "terms".into(),
                title: "Terms".into(),
                source: "terms.txt".into(),
                text: source,
                metadata: json!({}),
                chunking: json!({}),
                chunks,
            },
        })
        .unwrap();
    database
}

// Independent reference preserves the original eager String normalization and
// direct BM25 expression. The optimized builder must keep exact scores, not
// merely the same leading document or an approximate floating-point result.
fn original_tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}
fn reference_scores(texts: &[String], query: &str) -> Vec<f64> {
    let frequencies = texts
        .iter()
        .map(|text| {
            let mut counts = HashMap::<String, usize>::new();
            for word in original_tokens(text) {
                *counts.entry(word).or_default() += 1;
            }
            counts
        })
        .collect::<Vec<_>>();
    let lengths = frequencies
        .iter()
        .map(|counts| counts.values().sum::<usize>())
        .collect::<Vec<_>>();
    let mean = (lengths.iter().sum::<usize>() as f64 / lengths.len() as f64).max(f64::EPSILON);
    let terms = original_tokens(query).collect::<BTreeSet<_>>();
    let mut scores = vec![0.0; texts.len()];
    for term in terms {
        let document_frequency = frequencies
            .iter()
            .filter(|counts| counts.contains_key(&term))
            .count() as f64;
        if document_frequency == 0.0 {
            continue;
        }
        let idf = (1.0
            + (texts.len() as f64 - document_frequency + 0.5) / (document_frequency + 0.5))
            .ln();
        for (index, counts) in frequencies.iter().enumerate() {
            if let Some(&frequency) = counts.get(&term) {
                let tf = frequency as f64;
                let denominator = tf + 1.2 * (0.25 + 0.75 * lengths[index] as f64 / mean);
                scores[index] += idf * tf * 2.2 / denominator;
            }
        }
    }
    scores
}
fn compare(database: &Database, texts: &[String], query: &str) -> bool {
    let result = database
        .graph_rag_candidates(GraphRagRequest {
            collection: "lexical".into(),
            expected_profile: profile(),
            query: vectors::Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
            query_text: query.into(),
            candidate_limit: 100,
            seed_limit: 1,
            max_hops: 0,
            neighbor_limit: 4,
            vector_weight: 0.0,
            lexical_weight: 1.0,
        })
        .unwrap();
    let mut expected = reference_scores(texts, query)
        .into_iter()
        .enumerate()
        .filter(|(_, score)| *score > 0.0)
        .map(|(row, score)| (format!("5:terms:{row}"), row, score))
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| {
        right
            .2
            .total_cmp(&left.2)
            .then_with(|| left.0.cmp(&right.0))
    });
    assert_eq!(result.candidates.len(), expected.len(), "query {query:?}");
    for (rank, (candidate, (chunk_id, row, score))) in
        result.candidates.iter().zip(expected).enumerate()
    {
        assert_eq!(candidate.hit.chunk_id, chunk_id, "query {query:?}");
        assert_eq!(candidate.hit.text, texts[row]);
        assert_eq!(
            candidate.lexical_score.to_bits(),
            score.to_bits(),
            "query {query:?}, chunk {chunk_id}"
        );
        assert_eq!(
            candidate.fusion_score.to_bits(),
            (1.0 / (61.0 + rank as f64)).to_bits()
        );
    }
    result.lexical_cache_hit
}

#[test]
fn ascii_unicode_case_expansion_and_titlecase_preserve_original_bm25() {
    let texts = [
        "alpha alpha ALPHA Beta beta",
        "İstanbul İ I ı i\u{307}",
        "ǅ Ǆ ǆ ǅungla ǆungla",
        "ΟΣ ΟΣΟΣ οσ ος ΩΣ ωσ ως",
        "ŻÓŁĆ Żółć żółć café CAFÉ cafe\u{301}",
        "数据库检索 １２３ 123 ZX42 zx42 ZX42",
        "!!! --- ...",
    ]
    .map(str::to_owned)
    .to_vec();
    let database = database(&texts);
    for query in [
        "alpha beta",
        "İstanbul İ I ı",
        "ǅ Ǆ ǆungla",
        "ΟΣ ΟΣΟΣ ΩΣ",
        "ŻÓŁĆ café cafe\u{301}",
        "数据库检索 １２３ 123 zx42",
        "!!!",
    ] {
        compare(&database, &texts, query);
    }
}

#[test]
fn varied_term_frequencies_and_lengths_match_eager_reference_bit_for_bit() {
    let vocabulary = [
        "alpha", "ALPHA", "beta", "βήτα", "CAFÉ", "café", "ǅ", "İ", "ΟΣ", "storage", "ZX17", "17",
    ];
    let texts = (0..64)
        .map(|row| {
            (0..(3 + row * 7 % 83))
                .map(|position| {
                    vocabulary[(row * 13 + position * 7 + position * position) % vocabulary.len()]
                })
                .collect::<Vec<_>>()
                .join(if row % 2 == 0 { "..." } else { " \n " })
        })
        .collect::<Vec<_>>();
    let database = database(&texts);
    for query in [
        "alpha storage café 17",
        "βήτα ǅ İ ΟΣ",
        "ZX17 alpha alpha missing",
        "notpresent",
    ] {
        compare(&database, &texts, query);
        compare(&database, &texts, query);
    }
}

#[test]
fn oversized_vocabulary_fallback_preserves_unicode_query_scores() {
    let mut texts = (0..100)
        .map(|chunk| {
            (0..1000)
                .map(|term| format!("word{}", chunk * 1000 + term))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>();
    texts[99].push_str(" ŻÓŁĆ żółć ǅ ǆ İ ΟΣ");
    let database = database(&texts);
    for query in ["word42 ŻÓŁĆ ǅ İ ΟΣ", "word99999 word42", "missing"] {
        assert!(!compare(&database, &texts, query));
    }
}
