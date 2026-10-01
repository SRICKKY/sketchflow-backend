use std::collections::HashMap;

use axum::{extract::State, Json};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tower_cookies::Cookies;

use crate::{
    auth::session,
    billing::{
        self, activate_pro_subscription, get_billing_state, quote_checkout, razorpay,
        BillingInterval,
    },
    error::{AppError, AppResult},
    extract::AppJson,
    state::AppState,
};

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct CreateOrderRequest {
    /// `"month"` or `"year"`.
    #[serde(default)]
    pub interval: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreateOrderResponse {
    pub id: String,
    pub amount: i64,
    pub currency: String,
    pub key: String,
}

/// Create a Razorpay order to upgrade the signed-in user to Pro.
#[utoipa::path(
    post,
    path = "/api/create-order",
    tag = "billing",
    request_body = CreateOrderRequest,
    responses(
        (status = 200, description = "Order created", body = CreateOrderResponse),
        (status = 400, description = "Invalid interval or amount"),
        (status = 401, description = "Not signed in"),
        (status = 409, description = "Plan already active"),
        (status = 503, description = "Payment gateway not configured")
    )
)]
pub async fn create_order(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(body): AppJson<CreateOrderRequest>,
) -> AppResult<Json<CreateOrderResponse>> {
    let user = session::optional_current_user(&cookies, &state)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Sign in to upgrade to Pro.".to_string()))?;

    let interval_str = body.interval.as_deref().unwrap_or("");
    if interval_str != "month" && interval_str != "year" {
        return Err(AppError::BadRequest(
            "interval must be month or year.".to_string(),
        ));
    }
    let interval = BillingInterval::parse(interval_str);

    let billing_state = get_billing_state(&state.db, &user.id).await?;
    let now = Utc::now();
    let quote = quote_checkout(&state.config, interval, &billing_state, now);
    if quote.blocked {
        return Err(AppError::Conflict(
            quote
                .blocked_message
                .unwrap_or_else(|| "This plan is already active.".to_string()),
        ));
    }

    if quote.amount_minor < billing::RAZORPAY_MIN_AMOUNT {
        return Err(AppError::BadRequest(
            "Amount must be at least 100 (smallest currency unit).".to_string(),
        ));
    }

    let key = razorpay::checkout_key_id(&state.config)
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| {
            AppError::ServiceUnavailable("RAZORPAY_KEY_ID is not configured.".to_string())
        })?;

    let mut notes = HashMap::new();
    notes.insert("sketchflowUserId".to_string(), user.id.clone());
    notes.insert(
        "planInterval".to_string(),
        quote.interval.as_str().to_string(),
    );
    if quote.interval == BillingInterval::Year && interval == BillingInterval::Year {
        // Mirrors the Next.js `switchFrom: "month"` note when upgrading a
        // monthly plan to annual mid-cycle.
        if billing_state.interval == Some(BillingInterval::Month) {
            notes.insert("switchFrom".to_string(), "month".to_string());
        }
    }

    let receipt = format!(
        "sf_{}_{}",
        &user.id[user.id.len().saturating_sub(10)..],
        to_base36(now.timestamp_millis())
    );

    let order = razorpay::create_order(
        &state.config,
        quote.amount_minor,
        &state.config.checkout_currency,
        &receipt,
        notes,
    )
    .await?;

    Ok(Json(CreateOrderResponse {
        id: order.id,
        amount: order.amount,
        currency: order.currency,
        key,
    }))
}

fn to_base36(mut value: i64) -> String {
    if value == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    let negative = value < 0;
    if negative {
        value = -value;
    }
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    if negative {
        out.push(b'-');
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct VerifyPaymentRequest {
    #[serde(default)]
    pub razorpay_order_id: Option<String>,
    #[serde(default)]
    pub razorpay_payment_id: Option<String>,
    #[serde(default)]
    pub razorpay_signature: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct VerifyPaymentResponse {
    pub success: bool,
    pub status: String,
    pub plan: String,
    pub interval: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
}

/// Verify a Razorpay payment signature and activate the Pro subscription.
#[utoipa::path(
    post,
    path = "/api/verify-payment",
    tag = "billing",
    request_body = VerifyPaymentRequest,
    responses(
        (status = 200, description = "Payment verified and plan activated", body = VerifyPaymentResponse),
        (status = 400, description = "Invalid signature, payload, or payment state"),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Payment belongs to a different user")
    )
)]
pub async fn verify_payment(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(body): AppJson<VerifyPaymentRequest>,
) -> AppResult<Json<VerifyPaymentResponse>> {
    let user = session::optional_current_user(&cookies, &state)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Sign in to verify payment.".to_string()))?;

    let order_id = body.razorpay_order_id.as_deref().unwrap_or("").trim();
    let payment_id = body.razorpay_payment_id.as_deref().unwrap_or("").trim();
    let signature = body.razorpay_signature.as_deref().unwrap_or("").trim();
    if order_id.is_empty() || payment_id.is_empty() || signature.is_empty() {
        return Err(AppError::BadRequest(
            "razorpay_order_id, razorpay_payment_id, and razorpay_signature are required."
                .to_string(),
        ));
    }

    let key_secret = state
        .config
        .razorpay_key_secret
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            AppError::ServiceUnavailable(
                "RAZORPAY_KEY_ID and RAZORPAY_KEY_SECRET are not configured".to_string(),
            )
        })?;
    let expected = razorpay::payment_signature(order_id, payment_id, key_secret);
    if !razorpay::signatures_match(&expected, signature) {
        return Err(AppError::BadRequest(
            "Invalid payment signature.".to_string(),
        ));
    }

    let (order, payment) = tokio::try_join!(
        razorpay::fetch_order(&state.config, order_id),
        razorpay::fetch_payment(&state.config, payment_id),
    )?;

    if payment.order_id != order_id {
        return Err(AppError::BadRequest(
            "Payment does not match this order.".to_string(),
        ));
    }
    if !razorpay::is_captured_payment(&payment.status) {
        return Err(AppError::BadRequest(format!(
            "Payment is {}, not captured.",
            payment.status
        )));
    }

    if let Some(owner_id) = order.notes.get("sketchflowUserId") {
        if owner_id != &user.id {
            return Err(AppError::Forbidden(
                "Payment does not belong to this user.".to_string(),
            ));
        }
    }

    let interval = BillingInterval::parse_opt(order.notes.get("planInterval").map(String::as_str));

    let activated = activate_pro_subscription(
        &state.db,
        &user.id,
        interval,
        payment_id,
        order_id,
        payment_id,
        payment.amount,
        &payment.currency,
    )
    .await?;

    Ok(Json(VerifyPaymentResponse {
        success: true,
        status: payment.status,
        plan: "pro".to_string(),
        interval: activated.interval.as_str().to_string(),
        expires_at: activated
            .expires_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    }))
}
