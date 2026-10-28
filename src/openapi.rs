use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::routes::{
    auth, billing, collaboration, health, invoices, notifications, profile, uploads,
};

/// Aggregated OpenAPI document for the SketchFlow backend. Each module's
/// handlers are annotated with `#[utoipa::path(...)]` and registered here so
/// a single `/docs` Swagger UI reflects the whole API.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "SketchFlow API",
        version = "0.1.0",
        description = "REST API for SketchFlow: auth, billing, profile, board collaboration, notifications, uploads and invoices.",
        license(name = "UNLICENSED")
    ),
    tags(
        (name = "health", description = "Service health"),
        (name = "auth", description = "Authentication and session management"),
        (name = "billing", description = "Razorpay and Paddle checkout, webhooks, and subscription activation"),
        (name = "profile", description = "User profile, credentials, and avatar management"),
        (name = "uploads", description = "Board image asset uploads"),
        (name = "collaboration", description = "Board snapshots and live collaboration"),
        (name = "notifications", description = "In-app notifications"),
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
        billing::verify_paddle_payment,
        billing::paddle_webhook,
        profile::update_profile_name,
        profile::update_profile_password,
        profile::upload_avatar,
        profile::delete_avatar,
        uploads::upload_image,
        uploads::get_asset,
        collaboration::snapshot,
        notifications::list_notifications,
        notifications::unread_notification_count,
        notifications::mark_notification_read,
        notifications::mark_all_notifications_read,
        invoices::download_invoice_pdf
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
        billing::PaddleVerifyPaymentRequest,
        billing::PaddleWebhookResponse,
        crate::billing::paddle::PaddleCheckout,
        crate::billing::paddle::PaddleCompletion,
        profile::UpdateProfileNameRequest,
        profile::ProfileNameResponse,
        profile::UpdateProfilePasswordRequest,
        profile::ProfilePasswordResponse,
        profile::AvatarResponse,
        uploads::UploadResponse,
        collaboration::SnapshotResponse,
        notifications::NotificationType,
        notifications::NotificationItem,
        notifications::NotificationPage,
        notifications::UnreadCount,
        notifications::MarkReadResponse,
        notifications::MarkAllReadInput,
        notifications::MarkAllReadResponse,
        crate::models::PublicUser
    ))
)]
pub struct ApiDoc;

/// Builds the Swagger UI router, served at `/docs` with the spec at
/// `/api-docs/openapi.json`.
pub fn swagger_ui() -> SwaggerUi {
    SwaggerUi::new("/docs").url("/api-docs/openapi.json", ApiDoc::openapi())
}
