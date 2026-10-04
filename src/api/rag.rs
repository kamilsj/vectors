//! Bounded graph exploration and hybrid RAG retrieval over coherent snapshots.

use std::time::Instant;

use super::*;
use crate::{
    GraphBrowseRequest, GraphNeighborhoodDirection, GraphNeighborhoodRequest, GraphRagOptions,
    GraphRagRequest, GraphRagResult, GraphRagSelection, GraphRagTraversal,
    GraphRelationshipDeleteRequest, GraphRelationshipRequest, RerankingService,
};

#[path = "rag_chat.rs"]
mod chat;

pub(super) fn configure(config: &mut web::ServiceConfig) {
    config
        .route("/collections/{collection}/graph", web::get().to(browse))
        .route(
            "/collections/{collection}/neighborhood",
            web::get().to(neighborhood),
        )
        .route(
            "/collections/{collection}/retrieve",
            web::post().to(retrieve),
        )
        .route("/collections/{collection}/chat", web::post().to(chat::chat))
        .service(
            web::resource("/collections/{collection}/relationships")
                .route(web::post().to(upsert_relationship))
                .route(web::delete().to(delete_relationship)),
        );
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Browse {
    document_id: Option<String>,
    #[serde(default)]
    offset: usize,
    #[serde(default = "browse_limit")]
    limit: usize,
    #[serde(default = "edge_limit")]
    max_edges: usize,
}
fn browse_limit() -> usize {
    100
}
fn edge_limit() -> usize {
    500
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Neighborhood {
    chunk_id: String,
    #[serde(default = "default_hops")]
    max_hops: usize,
    #[serde(default = "default_neighbor_limit")]
    neighbor_limit: usize,
    #[serde(default = "browse_limit")]
    max_nodes: usize,
    #[serde(default = "edge_limit")]
    max_edges: usize,
    #[serde(default)]
    direction: GraphNeighborhoodDirection,
    kind: Option<String>,
    #[serde(default)]
    min_weight: f64,
}

async fn neighborhood(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    limits: web::Data<RequestLimits>,
    database: web::Data<Database>,
    collection: web::Path<String>,
    input: web::Query<Neighborhood>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let input = input.into_inner();
    if !(1..=200).contains(&input.max_nodes) || input.max_edges > 2000 {
        return Err(ApiError::bad_request(
            "invalid_graph_neighborhood",
            "graph neighborhoods accept 1..200 nodes and 0..2000 edges",
        ));
    }
    let database = database.get_ref().clone();
    let collection = collection.into_inner();
    let body = run_database_task(limiter.as_ref(), move || {
        encoded(&database.graph_neighborhood(GraphNeighborhoodRequest {
            collection,
            chunk_id: input.chunk_id,
            max_hops: input.max_hops,
            neighbor_limit: input.neighbor_limit,
            max_nodes: input.max_nodes.min(limits.max_response_rows),
            max_edges: input.max_edges.min(limits.max_response_rows),
            direction: input.direction,
            kind: input.kind,
            min_weight: input.min_weight,
        })?)
    })
    .await?;
    Ok(json_body(body))
}

async fn browse(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    limits: web::Data<RequestLimits>,
    database: web::Data<Database>,
    collection: web::Path<String>,
    input: web::Query<Browse>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let input = input.into_inner();
    if !(1..=200).contains(&input.limit) || input.max_edges > 2000 {
        return Err(ApiError::bad_request(
            "invalid_graph_browse",
            "graph browsing accepts 1..200 nodes and 0..2000 edges per page",
        ));
    }
    // Report the effective page size so clients can paginate under smaller
    // server limits without first fetching a separate settings endpoint.
    let node_limit = input.limit.min(limits.max_response_rows);
    let max_edges = input.max_edges.min(limits.max_response_rows);
    let database = database.get_ref().clone();
    let collection = collection.into_inner();
    let body = run_database_task(limiter.as_ref(), move || {
        encoded(&database.graph_browse(GraphBrowseRequest {
            collection,
            document_id: input.document_id,
            offset: input.offset,
            limit: node_limit,
            max_edges,
        })?)
    })
    .await?;
    Ok(json_body(body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Relationship {
    expected_revision: u64,
    from_chunk: String,
    to_chunk: String,
    kind: String,
    weight: f64,
}

async fn upsert_relationship(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    collection: web::Path<String>,
    input: web::Json<Relationship>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let input = input.into_inner();
    let database = database.get_ref().clone();
    let collection = collection.into_inner();
    let body = run_database_task(limiter.as_ref(), move || {
        encoded(
            &database.graph_upsert_relationship(GraphRelationshipRequest {
                collection,
                expected_revision: input.expected_revision,
                from_chunk: input.from_chunk,
                to_chunk: input.to_chunk,
                kind: input.kind,
                weight: input.weight,
            })?,
        )
    })
    .await?;
    Ok(json_body(body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRelationship {
    expected_revision: u64,
    from_chunk: String,
    to_chunk: String,
    kind: String,
}

async fn delete_relationship(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    collection: web::Path<String>,
    input: web::Json<DeleteRelationship>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let input = input.into_inner();
    let database = database.get_ref().clone();
    let collection = collection.into_inner();
    let body = run_database_task(limiter.as_ref(), move || {
        encoded(
            &database.graph_delete_relationship(GraphRelationshipDeleteRequest {
                collection,
                expected_revision: input.expected_revision,
                from_chunk: input.from_chunk,
                to_chunk: input.to_chunk,
                kind: input.kind,
            })?,
        )
    })
    .await?;
    Ok(json_body(body))
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Reranker {
    #[default]
    Local,
    Voyage,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Retrieve {
    text: String,
    #[serde(default)]
    document_filters: Vec<SearchFilter>,
    #[serde(default = "candidate_limit")]
    candidate_limit: usize,
    #[serde(default = "seed_limit")]
    seed_limit: usize,
    max_seeds_per_document: Option<usize>,
    #[serde(default = "result_limit")]
    max_results: usize,
    #[serde(default = "default_hops")]
    max_hops: usize,
    #[serde(default = "default_neighbor_limit")]
    neighbor_limit: usize,
    #[serde(default = "retrieval_direction")]
    direction: GraphNeighborhoodDirection,
    kind: Option<String>,
    #[serde(default)]
    min_weight: f64,
    #[serde(default = "diversity")]
    diversity: f64,
    #[serde(default = "context_limit")]
    max_context_bytes: usize,
    #[serde(default = "per_document")]
    max_per_document: usize,
    #[serde(default = "weight")]
    vector_weight: f64,
    #[serde(default = "weight")]
    lexical_weight: f64,
    #[serde(default)]
    reranker: Reranker,
}
fn candidate_limit() -> usize {
    40
}
fn seed_limit() -> usize {
    12
}
fn result_limit() -> usize {
    10
}
fn diversity() -> f64 {
    0.3
}
fn context_limit() -> usize {
    24_000
}
fn per_document() -> usize {
    3
}
fn weight() -> f64 {
    1.0
}
fn retrieval_direction() -> GraphNeighborhoodDirection {
    GraphNeighborhoodDirection::Outgoing
}

impl Retrieve {
    fn validate(&self, limits: &RequestLimits) -> Result<(), ApiError> {
        if self.text.trim().is_empty()
            || self.text.contains('\0')
            || self.text.len() > MAX_EMBEDDING_TEXT_BYTES
            || !(1..=100).contains(&self.candidate_limit)
            || !(1..=20).contains(&self.seed_limit)
            || self.seed_limit > self.candidate_limit
            || self
                .max_seeds_per_document
                .is_some_and(|limit| !(1..=20).contains(&limit))
            || self.max_hops > 3
            || !(1..=32).contains(&self.neighbor_limit)
            || self.max_results == 0
            || self.max_results > self.candidate_limit
            || self.max_results > limits.max_response_rows.min(100)
            || !self.diversity.is_finite()
            || !(0.0..=1.0).contains(&self.diversity)
            || !(1..=1024 * 1024).contains(&self.max_context_bytes)
            || !(1..=100).contains(&self.max_per_document)
            || !self.vector_weight.is_finite()
            || !(0.0..=10.0).contains(&self.vector_weight)
            || !self.lexical_weight.is_finite()
            || !(0.0..=10.0).contains(&self.lexical_weight)
            || self.vector_weight + self.lexical_weight == 0.0
        {
            return Err(ApiError::bad_request("invalid_rag_request", "use text up to 8191 bytes, 1..100 candidates, 1..20 seeds, an optional 1..20 seeds per document, 0..3 hops, 1..32 neighbors, results within candidate/server limits, diversity 0..1, context 1..1048576 bytes, 1..100 chunks per document, and nonzero combined weights in 0..10"));
        }
        GraphRagRequest::validate_query_text(&self.text)?;
        if matches!(self.reranker, Reranker::Voyage)
            && self.text.len() > crate::reranking::MAX_QUERY_BYTES
        {
            return Err(ApiError::bad_request(
                "invalid_reranking_request",
                "Voyage reranking queries may contain at most 7872 UTF-8 bytes",
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct RerankingSummary {
    method: Reranker,
    model: Option<String>,
    total_tokens: u64,
}

#[derive(Serialize)]
struct RetrieveResponse {
    #[serde(flatten)]
    result: GraphRagResult,
    reranking: RerankingSummary,
    embedding_usage: crate::embedding::Usage,
    timings: RetrievalTimings,
}

/// Wall-clock server stages, including admission waits; no invented token speed.
#[derive(Clone, Copy, Default, Serialize)]
struct RetrievalTimings {
    embedding_ms: f64,
    search_ms: f64,
    reranking_ms: f64,
    selection_ms: f64,
    generation_ms: f64,
    total_ms: f64,
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

// Services are independently optional for applications embedding the API.
#[allow(clippy::too_many_arguments)]
async fn retrieve(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    limits: web::Data<RequestLimits>,
    database: web::Data<Database>,
    embeddings: Option<web::Data<EmbeddingService>>,
    reranking: Option<web::Data<RerankingService>>,
    collection: web::Path<String>,
    input: web::Json<Retrieve>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let result = retrieve_result(
        limiter.as_ref(),
        &limits,
        database.get_ref().clone(),
        embeddings,
        reranking,
        collection.into_inner(),
        input.into_inner(),
        false,
    )
    .await?;
    Ok(json_body(encoded(&result)?))
}

#[allow(clippy::too_many_arguments)]
async fn retrieve_result(
    limiter: Option<&web::Data<DatabaseTaskLimiter>>,
    limits: &RequestLimits,
    database: Database,
    embeddings: Option<web::Data<EmbeddingService>>,
    reranking: Option<web::Data<RerankingService>>,
    collection: String,
    mut input: Retrieve,
    require_generation: bool,
) -> Result<RetrieveResponse, ApiError> {
    let preflight = retrieval_preflight(limiter, limits, &database, collection, &mut input).await?;
    retrieve_prepared(
        limiter,
        limits,
        database,
        embeddings,
        reranking,
        input,
        require_generation,
        preflight,
    )
    .await
}

struct RetrievalPreflight {
    state: crate::GraphCollection,
    document_filters: Vec<VectorSearchFilter>,
    traversal: GraphRagTraversal,
    has_chunks: bool,
    search_ms: f64,
}

async fn retrieval_preflight(
    limiter: Option<&web::Data<DatabaseTaskLimiter>>,
    limits: &RequestLimits,
    database: &Database,
    collection: String,
    input: &mut Retrieve,
) -> Result<RetrievalPreflight, ApiError> {
    input.validate(limits)?;
    if input.document_filters.len() > 32 {
        return Err(ApiError::bad_request(
            "invalid_rag_request",
            "RAG accepts at most 32 document filters",
        ));
    }
    let document_filters = typed_search_filters(std::mem::take(&mut input.document_filters))?;
    let traversal = GraphRagTraversal {
        direction: input.direction,
        kind: input.kind.take(),
        min_weight: input.min_weight,
    };
    traversal
        .validate()
        .map_err(|error| ApiError::bad_request("invalid_rag_request", error.to_string()))?;
    let read_db = database.clone();
    let preflight_filters = document_filters.clone();
    let search_started = Instant::now();
    let (state, has_chunks) = run_database_task(limiter, move || {
        read_db
            .graph_preflight_document_filters(&collection, &preflight_filters)
            .map_err(search_error)
    })
    .await?;

    Ok(RetrievalPreflight {
        state,
        document_filters,
        traversal,
        has_chunks,
        search_ms: elapsed_ms(search_started),
    })
}

fn empty_retrieval(state: &crate::GraphCollection, reranker: Reranker) -> RetrieveResponse {
    RetrieveResponse {
        result: GraphRagResult {
            collection: state.config.name.clone(),
            revision: state.revision,
            hits: Vec::new(),
            edges: Vec::new(),
            traversal_seed_ids: Vec::new(),
            lexical_cache_hit: false,
            truncated: false,
            context_bytes: 0,
            candidate_count: 0,
        },
        reranking: RerankingSummary {
            method: reranker,
            model: None,
            total_tokens: 0,
        },
        embedding_usage: crate::embedding::Usage::default(),
        timings: RetrievalTimings::default(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn retrieve_prepared(
    limiter: Option<&web::Data<DatabaseTaskLimiter>>,
    limits: &RequestLimits,
    database: Database,
    embeddings: Option<web::Data<EmbeddingService>>,
    reranking: Option<web::Data<RerankingService>>,
    input: Retrieve,
    require_generation: bool,
    preflight: RetrievalPreflight,
) -> Result<RetrieveResponse, ApiError> {
    let started = Instant::now();
    input.validate(limits)?;
    let RetrievalPreflight {
        state,
        document_filters,
        traversal,
        has_chunks,
        search_ms,
    } = preflight;
    if !has_chunks {
        return Ok(empty_retrieval(&state, input.reranker));
    }
    let mut timings = RetrievalTimings {
        search_ms,
        ..RetrievalTimings::default()
    };
    let max_edges = limits.max_response_rows;
    if require_generation {
        chat::ensure_configured(embeddings.as_ref())?;
    }
    let reranker = if matches!(input.reranker, Reranker::Voyage) {
        let service = crate::api::reranking::service(reranking)?;
        service.ensure_configured()?;
        Some(service)
    } else {
        None
    };
    let (query, usage) = if input.vector_weight > 0.0 {
        let embedding_started = Instant::now();
        let generated = embedding_service(embeddings)?
            .generate(GenerateRequest::pinned(
                vec![input.text.clone()],
                InputType::Query,
                expected_profile(&state.config.profile)?,
            ))
            .await?;
        timings.embedding_ms = elapsed_ms(embedding_started);
        if graph_profile(generated.profile()) != state.config.profile
            || !matches!(generated.input_type, InputType::Query)
        {
            return Err(ApiError::internal("query embedding profile changed"));
        }
        let values = generated
            .embeddings
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::internal("query embedding is missing"))?;
        (normalized_embedding(values)?, generated.usage)
    } else {
        // The engine ignores the query vector entirely in lexical-only mode;
        // matching dimensions still detect collection/profile changes. The
        // zero norm also catches accidental query-cosine work in regressions.
        (
            Vector::new(vec![0.0; state.config.profile.dimensions])?,
            crate::embedding::Usage::default(),
        )
    };
    let query_text = input.text.clone();
    let search_started = Instant::now();
    let snapshot = run_database_task(limiter, move || {
        let snapshot = database
            .graph_rag_candidates_with_options(
                GraphRagRequest {
                    collection: state.config.name,
                    expected_profile: state.config.profile,
                    query,
                    query_text,
                    candidate_limit: input.candidate_limit,
                    seed_limit: input.seed_limit,
                    max_hops: input.max_hops,
                    neighbor_limit: input.neighbor_limit,
                    vector_weight: input.vector_weight,
                    lexical_weight: input.lexical_weight,
                },
                GraphRagOptions {
                    traversal,
                    document_filters,
                    max_seeds_per_document: input.max_seeds_per_document,
                },
            )
            .map_err(search_error)?;
        Ok::<_, ApiError>(snapshot)
    })
    .await?;
    timings.search_ms += elapsed_ms(search_started);
    let candidate_count = snapshot.candidates.len();
    let reranking_started = Instant::now();
    let externally_reranked = reranker.is_some() && candidate_count > 0;
    let (scores, summary) = if let Some(service) = reranker.filter(|_| candidate_count > 0) {
        let documents = snapshot
            .candidates
            .iter()
            .map(|candidate| candidate.rerank_text.clone())
            .collect();
        let reranked = service.rerank(input.text, documents).await?;
        let mut scores = vec![0.0; candidate_count];
        for result in reranked.results {
            *scores
                .get_mut(result.index)
                .ok_or(crate::reranking::RerankingError::InvalidResponse)? = result.relevance_score;
        }
        (
            Some(scores),
            RerankingSummary {
                method: Reranker::Voyage,
                model: Some(reranked.model),
                total_tokens: reranked.usage.total_tokens,
            },
        )
    } else {
        (
            None,
            RerankingSummary {
                method: input.reranker,
                model: None,
                total_tokens: 0,
            },
        )
    };
    if externally_reranked {
        timings.reranking_ms = elapsed_ms(reranking_started);
    }
    let selection_started = Instant::now();
    let result = run_database_task(limiter, move || {
        let mut result = snapshot.finalize(
            GraphRagSelection {
                limit: input.max_results,
                diversity: input.diversity,
                max_context_bytes: input.max_context_bytes,
                max_per_document: input.max_per_document,
            },
            scores.as_deref(),
        )?;
        if result.edges.len() > max_edges {
            result.edges.truncate(max_edges);
            result.truncated = true;
        }
        Ok::<_, ApiError>(result)
    })
    .await?;
    timings.selection_ms = elapsed_ms(selection_started);
    timings.total_ms = search_ms + elapsed_ms(started);
    Ok(RetrieveResponse {
        result,
        reranking: summary,
        embedding_usage: usage,
        timings,
    })
}

#[cfg(test)]
#[path = "rag_tests.rs"]
mod tests;
