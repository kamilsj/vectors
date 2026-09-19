//! Authenticated provider settings and embedding generation routes.

use super::{authorize, ApiError, ApiSecurity};
use crate::embedding::{
    EmbeddingError, GenerateRequest, GenerateResponse, SettingsResponse, SettingsUpdate,
};
use crate::EmbeddingService;
use actix_web::{http::StatusCode, web, HttpRequest};

fn service(
    service: Option<web::Data<EmbeddingService>>,
) -> Result<web::Data<EmbeddingService>, ApiError> {
    service.ok_or(ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "embeddings_unavailable",
        message: "embedding service is not configured for this application".into(),
    })
}

pub(super) async fn get_settings(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    embeddings: Option<web::Data<EmbeddingService>>,
) -> Result<web::Json<SettingsResponse>, ApiError> {
    authorize(
        &request,
        security.as_ref().map(|security| security.get_ref()),
    )?;
    Ok(web::Json(service(embeddings)?.settings()?))
}

pub(super) async fn update_settings(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    embeddings: Option<web::Data<EmbeddingService>>,
    update: web::Json<SettingsUpdate>,
) -> Result<web::Json<SettingsResponse>, ApiError> {
    authorize(
        &request,
        security.as_ref().map(|security| security.get_ref()),
    )?;
    let service = service(embeddings)?;
    let permit = service.acquire_settings_update()?;
    let settings = web::block(move || {
        let _permit = permit;
        service.update_settings(update.into_inner())
    })
    .await
    .map_err(|_| ApiError::internal("embedding settings worker failed"))??;
    Ok(web::Json(settings))
}

pub(super) async fn generate_embeddings(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    embeddings: Option<web::Data<EmbeddingService>>,
    input: web::Json<GenerateRequest>,
) -> Result<web::Json<GenerateResponse>, ApiError> {
    authorize(
        &request,
        security.as_ref().map(|security| security.get_ref()),
    )?;
    Ok(web::Json(
        service(embeddings)?.generate(input.into_inner()).await?,
    ))
}

impl From<EmbeddingError> for ApiError {
    fn from(error: EmbeddingError) -> Self {
        let (status, code) = match error {
            EmbeddingError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_embedding_request"),
            EmbeddingError::NotConfigured => {
                (StatusCode::SERVICE_UNAVAILABLE, "embedding_not_configured")
            }
            EmbeddingError::Busy => (StatusCode::SERVICE_UNAVAILABLE, "embedding_overloaded"),
            EmbeddingError::SettingsChanged => (StatusCode::CONFLICT, "embedding_settings_changed"),
            EmbeddingError::Authentication => (StatusCode::BAD_GATEWAY, "embedding_auth_failed"),
            EmbeddingError::RateLimited => {
                (StatusCode::TOO_MANY_REQUESTS, "embedding_rate_limited")
            }
            EmbeddingError::Rejected => (StatusCode::BAD_REQUEST, "embedding_input_rejected"),
            EmbeddingError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "embedding_timeout"),
            EmbeddingError::Unavailable => (StatusCode::BAD_GATEWAY, "embedding_unavailable"),
            EmbeddingError::InvalidResponse => {
                (StatusCode::BAD_GATEWAY, "invalid_embedding_response")
            }
            EmbeddingError::Persistence | EmbeddingError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "embedding_settings_error",
            ),
        };
        Self {
            status,
            code,
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, App};
    use serde_json::{json, Value};

    #[actix_web::test]
    async fn embedding_routes_require_auth_and_missing_service_is_structured() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(ApiSecurity::bearer_token("test-admin")))
                .configure(crate::api::configure),
        )
        .await;
        for (method, path, payload) in [
            (
                actix_web::http::Method::GET,
                "/v1/settings/embeddings",
                json!({}),
            ),
            (
                actix_web::http::Method::PUT,
                "/v1/settings/embeddings",
                json!({"api_key":"synthetic-secret"}),
            ),
            (
                actix_web::http::Method::POST,
                "/v1/embeddings",
                json!({"input":["hello"]}),
            ),
        ] {
            let response = test::call_service(
                &app,
                test::TestRequest::default()
                    .method(method.clone())
                    .uri(path)
                    .set_json(&payload)
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let response = test::call_service(
                &app,
                test::TestRequest::default()
                    .method(method)
                    .uri(path)
                    .insert_header(("authorization", "Bearer test-admin"))
                    .set_json(&payload)
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body["error"]["code"], "embeddings_unavailable");
        }
    }

    #[actix_web::test]
    async fn keys_are_write_only_and_generation_rejects_stale_provider_selection() {
        let service = web::Data::new(crate::embedding::tests::test_service(None));
        let app = test::init_service(
            App::new()
                .app_data(service.clone())
                .configure(crate::api::configure),
        )
        .await;
        let permit = service.acquire_settings_update().unwrap();
        let response = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/v1/settings/embeddings")
                .set_json(json!({"api_key":"rejected-secret"}))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(permit);
        let response = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/v1/settings/embeddings")
                .set_json(json!({"api_key":"synthetic-secret", "dimensions":512}))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["configured"], true);
        assert_eq!(body["dimensions"], 512);
        assert!(!body.to_string().contains("synthetic-secret"));
        assert!(body.get("api_key").is_none());
        let body: Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/v1/settings/embeddings")
                .to_request(),
        )
        .await;
        assert!(body.get("api_key").is_none());
        assert_eq!(body["providers"][0]["configured"], true);
        let response = test::call_service(&app, test::TestRequest::post().uri("/v1/embeddings").set_json(json!({"input":["hello"],"expected_settings":{"provider":"openai","model":"text-embedding-3-small","dimensions":1536}})).to_request()).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"]["code"], "embedding_settings_changed");
    }
}
