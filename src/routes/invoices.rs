use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tower_cookies::Cookies;
use utoipa::ToSchema;

use crate::{
    auth::session,
    billing::{
        get_invoice_for_user, invoice_pdf::build_invoice_pdf, list_invoices_for_user, InvoiceRecord,
    },
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InvoiceResponse {
    pub id: String,
    pub number: String,
    pub interval: String,
    pub amount_minor: i32,
    pub currency: String,
    pub status: String,
    pub paid_at: Option<String>,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub gateway_order_id: Option<String>,
    pub gateway_payment_id: Option<String>,
}

impl From<InvoiceRecord> for InvoiceResponse {
    fn from(invoice: InvoiceRecord) -> Self {
        Self {
            id: invoice.id,
            number: invoice.number,
            interval: invoice.interval,
            amount_minor: invoice.amount_minor,
            currency: invoice.currency,
            status: invoice.status,
            paid_at: invoice.paid_at.map(|value| value.to_rfc3339()),
            period_start: invoice.period_start.map(|value| value.to_rfc3339()),
            period_end: invoice.period_end.map(|value| value.to_rfc3339()),
            gateway_order_id: invoice.gateway_order_id,
            gateway_payment_id: invoice.gateway_payment_id,
        }
    }
}

/// List the signed-in user's payment invoices.
#[utoipa::path(
    get,
    path = "/api/billing/invoices",
    tag = "invoices",
    responses(
        (status = 200, description = "Invoices ordered newest first", body = [InvoiceResponse]),
        (status = 401, description = "Not signed in")
    )
)]
pub async fn list_invoices(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<Vec<InvoiceResponse>>> {
    let user = session::optional_current_user(&cookies, &state)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Sign in to view invoices.".to_string()))?;
    let cache_key = crate::routes::billing::invoice_list_cache_key(&user.id);
    match state
        .cache
        .get_json::<Vec<InvoiceResponse>>(&cache_key)
        .await
    {
        Ok(Some(invoices)) => return Ok(Json(invoices)),
        Ok(None) => {}
        Err(error) => tracing::warn!(
            subsystem = "cache",
            event = "redis_cache_read_failed",
            cache = "billing_invoices",
            %error,
            "Loading invoices from PostgreSQL"
        ),
    }

    let invoices = list_invoices_for_user(&state.db, &state.config, &user.id).await?;
    let response: Vec<InvoiceResponse> = invoices.into_iter().map(InvoiceResponse::from).collect();
    if let Err(error) = state
        .cache
        .set_json(&cache_key, &response, Duration::from_secs(5 * 60 * 60))
        .await
    {
        tracing::warn!(
            subsystem = "cache",
            event = "redis_cache_write_failed",
            cache = "billing_invoices",
            %error,
            "Invoice list response was not cached"
        );
    }
    Ok(Json(response))
}

/// Download a paid invoice as a PDF. Mirrors
/// `app/billing/[invoiceId]/pdf/route.ts`, including its plain-text (not
/// JSON) error bodies.
#[utoipa::path(
    get,
    path = "/billing/{invoiceId}/pdf",
    tag = "billing",
    params(("invoiceId" = String, Path, description = "Invoice id")),
    responses(
        (status = 200, description = "Invoice PDF", content_type = "application/pdf"),
        (status = 401, description = "Sign in to download this invoice.", content_type = "text/plain"),
        (status = 404, description = "Invoice not found.", content_type = "text/plain")
    )
)]
pub async fn download_invoice_pdf(
    State(state): State<AppState>,
    cookies: Cookies,
    Path(invoice_id): Path<String>,
) -> Response {
    let user = match session::optional_current_user(&cookies, &state).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                "Sign in to download this invoice.",
            )
                .into_response();
        }
        Err(err) => {
            tracing::error!(
                subsystem = "auth",
                event = "invoice_session_resolution_failed",
                error = %err,
                "Failed to resolve invoice session"
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response();
        }
    };

    let invoice = match get_invoice_for_user(&state.db, &state.config, &user.id, &invoice_id).await
    {
        Ok(invoice) => invoice,
        Err(err) => {
            tracing::error!(
                subsystem = "database",
                event = "invoice_load_failed",
                error = %err,
                "Failed to load invoice"
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response();
        }
    };
    let Some(invoice) = invoice else {
        return (StatusCode::NOT_FOUND, "Invoice not found.").into_response();
    };

    let billed_to_name = user
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("SketchFlow customer");
    let bytes = build_invoice_pdf(&invoice, billed_to_name, Some(user.email.as_str()));

    let filename = format!("{}.pdf", invoice.number);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", filename),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        bytes,
    )
        .into_response()
}
