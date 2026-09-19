//! Authenticated, write-only credentials for optional cross-encoder reranking.

use super::{authorize, ApiError, ApiSecurity};
use crate::reranking::{RerankingError, SettingsResponse, SettingsUpdate};
use crate::RerankingService;
use actix_web::{http::StatusCode, web, HttpRequest};

pub(super) fn configure(config: &mut web::ServiceConfig) {
    config.service(
        web::resource("/settings/reranking")
            .route(web::get().to(get_settings))
            .route(web::put().to(update_settings)),
    );
}

pub(super) fn service(
    service: Option<web::Data<RerankingService>>,
) -> Result<web::Data<RerankingService>, ApiError> {
    service.ok_or(ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "reranking_unavailable",
        message: "reranking service is not configured for this application".into(),
    })
}

async fn get_settings(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    reranking: Option<web::Data<RerankingService>>,
) -> Result<web::Json<SettingsResponse>, ApiError> {
    authorize(
        &request,
        security.as_ref().map(|security| security.get_ref()),
    )?;
    Ok(web::Json(service(reranking)?.settings()?))
}

async fn update_settings(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    reranking: Option<web::Data<RerankingService>>,
    update: web::Json<SettingsUpdate>,
) -> Result<web::Json<SettingsResponse>, ApiError> {
    authorize(
        &request,
        security.as_ref().map(|security| security.get_ref()),
    )?;
    let service = service(reranking)?;
    let permit = service.acquire_settings_update()?;
    let settings = web::block(move || {
        let _permit = permit;
        service.update_settings(update.into_inner())
    })
    .await
    .map_err(|_| ApiError::internal("reranking settings worker failed"))??;
    Ok(web::Json(settings))
}

impl From<RerankingError> for ApiError {
    fn from(error: RerankingError) -> Self {
        let (status, code) = match error {
            RerankingError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_reranking_request"),
            RerankingError::NotConfigured => {
                (StatusCode::SERVICE_UNAVAILABLE, "reranking_not_configured")
            }
            RerankingError::Busy => (StatusCode::SERVICE_UNAVAILABLE, "reranking_overloaded"),
            RerankingError::Authentication => (StatusCode::BAD_GATEWAY, "reranking_auth_failed"),
            RerankingError::RateLimited => {
                (StatusCode::TOO_MANY_REQUESTS, "reranking_rate_limited")
            }
            RerankingError::Rejected => (StatusCode::BAD_REQUEST, "reranking_input_rejected"),
            RerankingError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "reranking_timeout"),
            RerankingError::Unavailable => (StatusCode::BAD_GATEWAY, "reranking_unavailable"),
            RerankingError::InvalidResponse => {
                (StatusCode::BAD_GATEWAY, "invalid_reranking_response")
            }
            RerankingError::Persistence | RerankingError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "reranking_settings_error",
            ),
        };
        Self {
            status,
            code,
            message: error.to_string(),
        }
    }
}
