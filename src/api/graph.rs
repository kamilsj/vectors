//! Document chunking, pinned provider embeddings, and SQL-backed graph retrieval.

use super::*;
use crate::chunking::{chunk_text, ChunkingConfig, TextChunk};
use crate::embedding::{ExpectedSettings, GenerateRequest, InputType};
use crate::{
    EmbeddingService, GraphChunkInput, GraphCollectionConfig, GraphDocumentInput,
    GraphEmbeddingProfile, GraphIngestRequest, GraphSearchRequest,
};

const CONTEXT_FORMAT_VERSION: u32 = 1;
// A conservative byte bound is also a token upper bound for the supported
// byte-based tokenizers. This includes the title/heading context we add.
const MAX_EMBEDDING_TEXT_BYTES: usize = 8191;
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;

#[path = "rag.rs"]
mod rag;

pub(super) fn configure(config: &mut web::ServiceConfig) {
    config.service(
        web::scope("/graph")
            .configure(rag::configure)
            .route("/chunk", web::post().to(preview_chunks))
            .service(
                web::resource("/collections")
                    .route(web::get().to(list_collections))
                    .route(web::post().to(create_collection)),
            )
            .route("/collections/{collection}", web::get().to(get_collection))
            .route(
                "/collections/{collection}/documents",
                web::post().to(ingest_document),
            )
            .service(
                web::resource("/collections/{collection}/documents/{document}")
                    .route(web::get().to(get_document))
                    .route(web::delete().to(delete_document)),
            )
            .route("/collections/{collection}/search", web::post().to(search)),
    );
}

fn embedding_service(
    service: Option<web::Data<EmbeddingService>>,
) -> Result<web::Data<EmbeddingService>, ApiError> {
    service.ok_or(ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "embeddings_unavailable",
        message: "embedding service is not configured for this application".into(),
    })
}

fn graph_profile(profile: ExpectedSettings) -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: profile.provider.as_str().into(),
        model: profile.model,
        dimensions: profile.dimensions,
        context_format_version: CONTEXT_FORMAT_VERSION,
    }
}

fn expected_profile(profile: &GraphEmbeddingProfile) -> Result<ExpectedSettings, ApiError> {
    if profile.context_format_version != CONTEXT_FORMAT_VERSION {
        return Err(ApiError::bad_request(
            "unsupported_context_format",
            "this collection uses an unsupported embedding context format",
        ));
    }
    Ok(ExpectedSettings::new(
        &profile.provider,
        &profile.model,
        profile.dimensions,
    )?)
}

fn encoded(value: &impl Serialize) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(value)
        .map_err(|error| ApiError::internal(format!("cannot encode graph response: {error}")))
}

fn json_body(body: Vec<u8>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(body)
}

fn normalized_embedding(values: Vec<f32>) -> Result<Vector, ApiError> {
    Vector::new(values)
        .and_then(|vector| vector.normalized())
        .map_err(|_| crate::embedding::EmbeddingError::InvalidResponse.into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkRequest {
    text: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    chunking: ChunkingConfig,
}

#[derive(Serialize)]
struct ChunkPreview {
    #[serde(flatten)]
    chunk: TextChunk,
    embedding_text: String,
}

#[derive(Serialize)]
struct ChunkResponse {
    chunking: ChunkingConfig,
    chunks: Vec<ChunkPreview>,
    embedding_bytes: usize,
    context_format_version: u32,
}

fn prepare_chunks(request: ChunkRequest) -> Result<ChunkResponse, ApiError> {
    if request.text.trim().is_empty() {
        return Err(ApiError::bad_request(
            "empty_document",
            "document text must not be empty",
        ));
    }
    if request.text.contains('\0') || request.title.contains('\0') {
        return Err(ApiError::bad_request(
            "invalid_document",
            "document text and title must not contain NUL characters",
        ));
    }
    if request.text.len() > MAX_DOCUMENT_BYTES || request.title.len() > 1024 {
        return Err(ApiError::bad_request(
            "document_too_large",
            "text may contain at most 1 MiB and title at most 1024 UTF-8 bytes",
        ));
    }
    let chunks = chunk_text(&request.text, &request.chunking)?;
    let mut embedding_bytes = 0usize;
    let chunks = chunks.into_iter().filter(|chunk| !chunk.text.trim().is_empty()).enumerate().map(|(ordinal, mut chunk)| {
        chunk.ordinal = ordinal;
        let mut embedding_text = String::new();
        if !request.title.trim().is_empty() {
            embedding_text.push_str("Title: ");
            embedding_text.push_str(&request.title);
            embedding_text.push('\n');
        }
        if let Some(heading) = &chunk.heading {
            embedding_text.push_str("Section: ");
            embedding_text.push_str(heading);
            embedding_text.push('\n');
        }
        if !embedding_text.is_empty() { embedding_text.push('\n'); }
        embedding_text.push_str(&chunk.text);
        if embedding_text.trim().is_empty() || embedding_text.len() > MAX_EMBEDDING_TEXT_BYTES {
            return Err(ApiError::bad_request("invalid_chunk_size", "each chunk plus its title and heading must contain nonempty text of at most 8191 UTF-8 bytes; reduce max_characters"));
        }
        embedding_bytes += embedding_text.len();
        Ok(ChunkPreview { chunk, embedding_text })
    }).collect::<Result<Vec<_>, ApiError>>()?;
    if embedding_bytes > MAX_DOCUMENT_BYTES {
        return Err(ApiError::bad_request(
            "embedding_input_too_large",
            "combined chunks and context exceed 1 MiB; use a smaller document or less overlap",
        ));
    }
    Ok(ChunkResponse {
        chunking: request.chunking,
        chunks,
        embedding_bytes,
        context_format_version: CONTEXT_FORMAT_VERSION,
    })
}

async fn preview_chunks(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    input: web::Json<ChunkRequest>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let input = input.into_inner();
    let body =
        run_database_task(limiter.as_ref(), move || encoded(&prepare_chunks(input)?)).await?;
    Ok(json_body(body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateCollection {
    name: String,
    #[serde(default)]
    document_columns: Vec<super::schema::CreateColumn>,
    #[serde(default = "default_semantic_neighbors")]
    semantic_neighbors: usize,
    #[serde(default = "default_semantic_threshold")]
    semantic_threshold: f64,
}
fn default_semantic_neighbors() -> usize {
    3
}
fn default_semantic_threshold() -> f64 {
    0.8
}

async fn create_collection(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    embeddings: Option<web::Data<EmbeddingService>>,
    input: web::Json<CreateCollection>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let profile = graph_profile(embedding_service(embeddings)?.profile()?);
    let input = input.into_inner();
    let columns = super::schema::decode_columns(input.document_columns, 0, 32)?;
    let database = database.get_ref().clone();
    let body = run_database_task(limiter.as_ref(), move || {
        encoded(&database.graph_create_collection_with_columns(
            GraphCollectionConfig {
                name: input.name,
                profile,
                semantic_neighbors: input.semantic_neighbors,
                semantic_threshold: input.semantic_threshold,
            },
            columns,
        )?)
    })
    .await?;
    Ok(json_body(body))
}

async fn list_collections(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let database = database.get_ref().clone();
    let body = run_database_task(limiter.as_ref(), move || {
        encoded(&database.graph_collections()?)
    })
    .await?;
    Ok(json_body(body))
}

async fn get_collection(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    collection: web::Path<String>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let database = database.get_ref().clone();
    let collection = collection.into_inner();
    let body = run_database_task(limiter.as_ref(), move || {
        encoded(&database.graph_collection(&collection)?)
    })
    .await?;
    Ok(json_body(body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestDocument {
    id: String,
    text: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    source: String,
    #[serde(default = "default_metadata")]
    metadata: JsonValue,
    #[serde(default)]
    chunking: ChunkingConfig,
    expected_revision: Option<u64>,
}
fn default_metadata() -> JsonValue {
    serde_json::json!({})
}

fn validate_document(input: &IngestDocument) -> Result<(), ApiError> {
    if input.id.trim().is_empty()
        || input.id.len() > 256
        || input.source.len() > 4096
        || input.id.contains('\0')
        || input.source.contains('\0')
        || !input.metadata.is_object()
        || input.metadata.to_string().len() > 16 * 1024
    {
        return Err(ApiError::bad_request("invalid_document", "use a nonempty id up to 256 bytes, source up to 4096 bytes, and a metadata object up to 16 KiB"));
    }
    Ok(())
}

struct PendingIngest {
    state: crate::GraphCollection,
    input: IngestDocument,
    preview: ChunkResponse,
    chunking: JsonValue,
}

enum PreparedIngest {
    Complete(Vec<u8>),
    Generate(Box<PendingIngest>),
}

fn canonical_document_metadata(
    metadata: &mut JsonValue,
    columns: &[crate::GraphDocumentColumn],
) -> Result<(), ApiError> {
    let values = metadata.as_object_mut().ok_or_else(|| {
        ApiError::bad_request("invalid_document", "document metadata must be an object")
    })?;
    for column in columns {
        let value = values.entry(column.name.clone()).or_insert(JsonValue::Null);
        let data_type = super::schema::parse_type(&column.data_type)?;
        *value = json_value(json_typed_value(value, &data_type, &column.name, false)?);
    }
    Ok(())
}

async fn ingest_document(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    embeddings: Option<web::Data<EmbeddingService>>,
    collection: web::Path<String>,
    input: web::Json<IngestDocument>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let mut input = input.into_inner();
    let collection = collection.into_inner();
    let database = database.get_ref().clone();
    let prepare_db = database.clone();
    let prepared = run_database_task(limiter.as_ref(), move || {
        validate_document(&input)?;
        let state = prepare_db.graph_collection(&collection)?;
        if let Some(expected) = input.expected_revision {
            if expected != state.revision {
                return Err(ApiError::from(Error::RevisionConflict {
                    expected,
                    actual: state.revision,
                }));
            }
        }
        canonical_document_metadata(&mut input.metadata, &state.document_columns)?;
        let preview = prepare_chunks(ChunkRequest {
            text: input.text.clone(),
            title: input.title.clone(),
            chunking: input.chunking,
        })?;
        let chunking = serde_json::to_value(input.chunking)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        let old = prepare_db.graph_document(&collection, &input.id)?;
        if let Some(old) = old.filter(|old| {
            old.chunks_intact
                && old.text == input.text
                && old.title == input.title
                && old.source == input.source
                && old.chunking == chunking
                && old.chunk_count == preview.chunks.len()
        }) {
            let actual = prepare_db.revision()?;
            if actual != state.revision {
                return Err(ApiError::from(Error::RevisionConflict {
                    expected: state.revision,
                    actual,
                }));
            }
            let unchanged = old.metadata == input.metadata;
            let revision = if unchanged {
                state.revision
            } else {
                prepare_db
                    .graph_update_document_metadata(
                        &collection,
                        &input.id,
                        input.metadata,
                        state.revision,
                    )?
                    .revision
            };
            return Ok::<_, ApiError>(PreparedIngest::Complete(encoded(&serde_json::json!({
                "collection": state.config.name, "document_id": input.id, "revision": revision,
                "chunks": old.chunk_count, "edges_created": 0, "replaced": !unchanged,
                "unchanged": unchanged, "embeddings_reused": true,
                "embedding_usage": { "total_tokens": 0 }
            }))?));
        }
        let validated_revision = prepare_db.graph_check_ingest_capacity(
            &collection,
            &crate::GraphDocumentPreview {
                id: &input.id,
                title: &input.title,
                source: &input.source,
                text: &input.text,
                metadata: &input.metadata,
                chunking: &chunking,
                chunks: preview
                    .chunks
                    .iter()
                    .map(|chunk| crate::GraphChunkPreview {
                        start_byte: chunk.chunk.byte_start,
                        end_byte: chunk.chunk.byte_end,
                        text: &chunk.chunk.text,
                        embedding_text: &chunk.embedding_text,
                    })
                    .collect(),
            },
        )?;
        if validated_revision != state.revision {
            return Err(ApiError::from(Error::RevisionConflict {
                expected: state.revision,
                actual: validated_revision,
            }));
        }
        Ok::<_, ApiError>(PreparedIngest::Generate(Box::new(PendingIngest {
            state,
            input,
            preview,
            chunking,
        })))
    })
    .await?;
    let pending = match prepared {
        PreparedIngest::Complete(body) => return Ok(json_body(body)),
        PreparedIngest::Generate(pending) => pending,
    };
    let PendingIngest {
        state,
        input,
        preview,
        chunking,
    } = *pending;
    let service = embedding_service(embeddings)?;
    let generated = service
        .generate(GenerateRequest::pinned(
            preview
                .chunks
                .iter()
                .map(|chunk| chunk.embedding_text.clone())
                .collect(),
            InputType::Document,
            expected_profile(&state.config.profile)?,
        ))
        .await?;
    if graph_profile(generated.profile()) != state.config.profile
        || !matches!(generated.input_type, InputType::Document)
    {
        return Err(ApiError::internal(
            "generated embeddings have an unexpected profile or input type",
        ));
    }
    let body = run_database_task(limiter.as_ref(), move || {
        if generated.embeddings.len() != preview.chunks.len() {
            return Err(ApiError::internal(
                "embedding provider returned the wrong chunk count",
            ));
        }
        let chunks = preview
            .chunks
            .into_iter()
            .zip(generated.embeddings)
            .map(|(chunk, embedding)| {
                Ok(GraphChunkInput {
                    start_byte: chunk.chunk.byte_start,
                    end_byte: chunk.chunk.byte_end,
                    text: chunk.chunk.text,
                    embedding_text: chunk.embedding_text,
                    embedding: normalized_embedding(embedding)?,
                })
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        let result = database.graph_ingest_document(GraphIngestRequest {
            collection: state.config.name,
            expected_revision: state.revision,
            expected_profile: state.config.profile,
            document: GraphDocumentInput {
                id: input.id,
                title: input.title,
                source: input.source,
                text: input.text,
                metadata: input.metadata,
                chunking,
                chunks,
            },
        })?;
        let mut response =
            serde_json::to_value(result).map_err(|error| ApiError::internal(error.to_string()))?;
        response["unchanged"] = JsonValue::Bool(false);
        response["embeddings_reused"] = JsonValue::Bool(false);
        response["embedding_usage"] = serde_json::to_value(generated.usage)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        encoded(&response)
    })
    .await?;
    Ok(json_body(body))
}

async fn get_document(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    path: web::Path<(String, String)>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let (collection, document) = path.into_inner();
    let database = database.get_ref().clone();
    let body = run_database_task(limiter.as_ref(), move || {
        let document = database
            .graph_document(&collection, &document)?
            .ok_or(ApiError {
                status: StatusCode::NOT_FOUND,
                code: "document_not_found",
                message: "document does not exist".into(),
            })?;
        encoded(&document)
    })
    .await?;
    Ok(json_body(body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteDocument {
    expected_revision: u64,
}

async fn delete_document(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    path: web::Path<(String, String)>,
    input: web::Json<DeleteDocument>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let (collection, document) = path.into_inner();
    let revision = input.expected_revision;
    let database = database.get_ref().clone();
    let body = run_database_task(limiter.as_ref(), move || {
        encoded(&database.graph_delete_document(&collection, &document, revision)?)
    })
    .await?;
    Ok(json_body(body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    text: String,
    #[serde(default = "default_seed_limit")]
    seed_limit: usize,
    #[serde(default = "default_hops")]
    max_hops: usize,
    #[serde(default = "default_neighbor_limit")]
    neighbor_limit: usize,
    #[serde(default = "default_max_results")]
    max_results: usize,
}
fn default_seed_limit() -> usize {
    5
}
fn default_hops() -> usize {
    1
}
fn default_neighbor_limit() -> usize {
    8
}
fn default_max_results() -> usize {
    20
}

// Keep Actix's independently configured extractors explicit at the route boundary.
#[allow(clippy::too_many_arguments)]
async fn search(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    limits: web::Data<RequestLimits>,
    database: web::Data<Database>,
    embeddings: Option<web::Data<EmbeddingService>>,
    collection: web::Path<String>,
    input: web::Json<Search>,
) -> Result<HttpResponse, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let input = input.into_inner();
    let maximum = limits.max_response_rows.min(100);
    if input.text.trim().is_empty()
        || input.text.len() > MAX_EMBEDDING_TEXT_BYTES
        || !(1..=20).contains(&input.seed_limit)
        || input.max_hops > 3
        || !(1..=32).contains(&input.neighbor_limit)
        || input.max_results == 0
        || input.max_results > maximum
        || input.seed_limit > input.max_results
    {
        return Err(ApiError::bad_request("invalid_graph_search", format!("use nonempty text up to 8191 bytes, seed_limit 1..20, max_hops 0..3, neighbor_limit 1..32, max_results 1..{maximum}, and seed_limit <= max_results")));
    }
    let collection = collection.into_inner();
    let database = database.get_ref().clone();
    let read_db = database.clone();
    let state = run_database_task(limiter.as_ref(), move || {
        read_db.graph_collection(&collection)
    })
    .await?;
    if state.chunk_count == 0 {
        return Ok(json_body(encoded(&crate::GraphSearchResult {
            collection: state.config.name,
            revision: state.revision,
            hits: Vec::new(),
            edges: Vec::new(),
            truncated: false,
        })?));
    }
    let generated = embedding_service(embeddings)?
        .generate(GenerateRequest::pinned(
            vec![input.text],
            InputType::Query,
            expected_profile(&state.config.profile)?,
        ))
        .await?;
    if graph_profile(generated.profile()) != state.config.profile
        || !matches!(generated.input_type, InputType::Query)
    {
        return Err(ApiError::internal(
            "generated query embedding has an unexpected profile or input type",
        ));
    }
    let body = run_database_task(limiter.as_ref(), move || {
        let query = generated
            .embeddings
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::internal("query embedding is missing"))?;
        let result = database.graph_search(GraphSearchRequest {
            collection: state.config.name,
            expected_profile: state.config.profile,
            query: normalized_embedding(query)?,
            seed_limit: input.seed_limit,
            max_hops: input.max_hops,
            neighbor_limit: input.neighbor_limit,
            max_results: input.max_results,
        })?;
        encoded(&result)
    })
    .await?;
    Ok(json_body(body))
}
