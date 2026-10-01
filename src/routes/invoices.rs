use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use tower_cookies::Cookies;

use crate::{
    auth::session, billing::get_invoice_for_user, billing::invoice_pdf::build_invoice_pdf,
    state::AppState,
};

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
            tracing::error!(error = %err, "failed to resolve session");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response();
        }
    };

    let invoice = match get_invoice_for_user(&state.db, &user.id, &invoice_id).await {
        Ok(invoice) => invoice,
        Err(err) => {
            tracing::error!(error = %err, "failed to load invoice");
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
