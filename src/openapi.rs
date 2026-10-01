use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::routes::{auth, billing, health, profile, uploads};

/// Aggregated OpenAPI document for the SketchFlow backend. Each module's
/// handlers are annotated with `#[utoipa::path(...)]` and registered here so
/// a single `/docs` Swagger UI reflects the whole API.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "SketchFlow API",
        version = "0.1.0",
        description = "REST API for SketchFlow: auth, billing, profile, board uploads and invoices.",
        license(name = "UNLICENSED")
    ),
    tags(
        (name = "health", description = "Service health"),
        (name = "auth", description = "Authentication and session management"),
        (name = "billing", description = "Razorpay checkout and subscription activation"),
        (name = "profile", description = "User profile and avatar management"),
        (name = "uploads", description = "Board image asset uploads"),
        (name = "invoices", description = "Invoice retrieval and PDF export")
    ),
    paths(
        health::health,
        auth::register,
        auth::login,
        auth::logout,
        auth::get_session,
        billing::create_order,
        billing::verify_payment,
        profile::upload_avatar,
        profile::delete_avatar,
        uploads::upload_image,
        uploads::get_asset
    ),
    components(schemas(
        health::HealthResponse,
        auth::RegisterRequest,
        auth::LoginRequest,
        auth::AuthResponse,
        auth::SessionResponse,
        auth::LogoutResponse,
        billing::CreateOrderRequest,
        billing::CreateOrderResponse,
        billing::VerifyPaymentRequest,
        billing::VerifyPaymentResponse,
        profile::AvatarResponse,
        uploads::UploadResponse,
        crate::models::PublicUser
    ))
)]
pub struct ApiDoc;

/// Builds the Swagger UI router, served at `/docs` with the spec at
/// `/api-docs/openapi.json`.
pub fn swagger_ui() -> SwaggerUi {
    SwaggerUi::new("/docs").url("/api-docs/openapi.json", ApiDoc::openapi())
}
