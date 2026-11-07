use std::collections::HashMap;
use std::time::Instant;

use axum::{body::Bytes, extract::State, http::HeaderMap, Json};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_cookies::Cookies;

use crate::{
    auth::session,
    billing::{
        self, activate_pro_subscription, get_billing_state, quote_checkout, razorpay,
        BillingInterval, BillingStatus,
    },
    error::{AppError, AppResult},
    extract::AppJson,
    state::AppState,
};

struct PaymentAttempt {
    provider: String,
    operation: &'static str,
    started: Instant,
    event_type: Option<String>,
    succeeded: bool,
}

impl PaymentAttempt {
    fn new(provider: impl Into<String>, operation: &'static str) -> Self {
        Self {
            provider: provider.into(),
            operation,
            started: Instant::now(),
            event_type: None,
            succeeded: false,
        }
    }

    fn set_event_type(&mut self, event_type: &str) {
        self.event_type = Some(event_type.to_owned());
    }

    fn succeed(mut self) {
        self.succeeded = true;
        tracing::info!(
            subsystem = "payments",
            event = "payment_operation_succeeded",
            provider = %self.provider,
            operation = self.operation,
            event_type = self.event_type.as_deref().unwrap_or("none"),
            duration_ms = self.started.elapsed().as_millis() as u64,
            "Payment operation succeeded"
        );
    }
}

impl Drop for PaymentAttempt {
    fn drop(&mut self) {
        if !self.succeeded {
            tracing::warn!(
                subsystem = "payments",
                event = "payment_operation_failed",
                provider = %self.provider,
                operation = self.operation,
                event_type = self.event_type.as_deref().unwrap_or("unknown"),
                duration_ms = self.started.elapsed().as_millis() as u64,
                "Payment operation failed"
            );
        }
    }
}

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct CreateOrderRequest {
    /// `"month"` or `"year"`.
    #[serde(default)]
    pub interval: Option<String>,
    /// `"lifetime"` for the one-time Lifetime Pro purchase.
    #[serde(default)]
    pub plan: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreateOrderResponse {
    pub gateway: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(rename = "transactionId", skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    #[serde(rename = "clientToken", skip_serializing_if = "Option::is_none")]
    pub client_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AnnualUpgradeQuoteResponse {
    pub eligible: bool,
    #[serde(rename = "currentInterval")]
    pub current_interval: Option<String>,
    #[serde(rename = "currentExpiresAt")]
    pub current_expires_at: Option<String>,
    #[serde(rename = "annualPriceMinor")]
    pub annual_price_minor: Option<i64>,
    #[serde(rename = "unusedCreditMinor")]
    pub unused_credit_minor: Option<i64>,
    #[serde(rename = "amountDueMinor")]
    pub amount_due_minor: Option<i64>,
    pub currency: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BillingOverviewResponse {
    pub status: String,
    pub plan: String,
    pub interval: Option<String>,
    #[serde(rename = "lifetimePro")]
    pub lifetime_pro: bool,
    pub starts_at: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LifetimeUpgradeQuoteResponse {
    pub eligible: bool,
    #[serde(rename = "lifetimePriceMinor")]
    pub lifetime_price_minor: Option<i64>,
    #[serde(rename = "unusedCreditMinor")]
    pub unused_credit_minor: Option<i64>,
    #[serde(rename = "amountDueMinor")]
    pub amount_due_minor: Option<i64>,
    pub currency: Option<String>,
}

/// Return the signed-in user's current billing period and plan status.
#[utoipa::path(
    get,
    path = "/api/billing/overview",
    tag = "billing",
    responses(
        (status = 200, description = "Billing overview", body = BillingOverviewResponse),
        (status = 401, description = "Not signed in")
    )
)]
pub async fn billing_overview(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<BillingOverviewResponse>> {
    let user = session::optional_current_user(&cookies, &state)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Sign in to view billing.".to_string()))?;
    let billing = get_billing_state(&state.db, &user.id).await?;
    let status = match billing.status {
        BillingStatus::Free => "free",
        BillingStatus::Active => "active",
        BillingStatus::Expired => "expired",
    };

    Ok(Json(BillingOverviewResponse {
        status: status.to_string(),
        plan: billing.plan.to_string(),
        interval: billing.interval.map(|value| value.as_str().to_string()),
        lifetime_pro: billing.lifetime_pro,
        starts_at: billing.starts_at.map(|value| value.to_rfc3339()),
        expires_at: billing.expires_at.map(|value| value.to_rfc3339()),
    }))
}

/// Return an exact quote for Lifetime Pro, including any unused paid-time credit.
#[utoipa::path(
    get,
    path = "/api/billing/lifetime-upgrade-quote",
    tag = "billing",
    responses(
        (status = 200, description = "Lifetime Pro checkout quote", body = LifetimeUpgradeQuoteResponse),
        (status = 401, description = "Not signed in"),
        (status = 503, description = "Lifetime Pro price is invalid")
    )
)]
pub async fn lifetime_upgrade_quote(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<LifetimeUpgradeQuoteResponse>> {
    let user = session::optional_current_user(&cookies, &state)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Sign in to view Lifetime Pro pricing.".into()))?;
    let billing_state = get_billing_state(&state.db, &user.id).await?;
    let quote = billing::quote_lifetime_checkout(&state.config, &billing_state, Utc::now());
    if quote.blocked {
        return Ok(Json(LifetimeUpgradeQuoteResponse {
            eligible: false,
            lifetime_price_minor: None,
            unused_credit_minor: None,
            amount_due_minor: None,
            currency: None,
        }));
    }

    let lifetime_price_minor =
        billing::paddle::checkout_amount_to_paddle_minor(state.config.plan_price_lifetime_minor)
            .ok_or_else(|| {
                AppError::ServiceUnavailable("Lifetime Pro price is not configured.".to_string())
            })?;
    let amount_due_minor = billing::paddle::checkout_amount_to_paddle_minor(quote.amount_minor)
        .ok_or_else(|| {
            AppError::ServiceUnavailable("Lifetime Pro amount is invalid.".to_string())
        })?;
    let unused_credit_minor = lifetime_price_minor
        .checked_sub(amount_due_minor)
        .filter(|credit| *credit >= 0)
        .ok_or_else(|| {
            AppError::ServiceUnavailable("Lifetime Pro quote is inconsistent.".to_string())
        })?;

    Ok(Json(LifetimeUpgradeQuoteResponse {
        eligible: true,
        lifetime_price_minor: Some(lifetime_price_minor),
        unused_credit_minor: Some(unused_credit_minor),
        amount_due_minor: Some(amount_due_minor),
        currency: Some("USD".to_string()),
    }))
}

/// Return an exact, read-only quote for an active monthly Pro user's annual upgrade.
#[utoipa::path(
    get,
    path = "/api/billing/annual-upgrade-quote",
    tag = "billing",
    responses(
        (status = 200, description = "Annual upgrade quote or an ineligible result", body = AnnualUpgradeQuoteResponse),
        (status = 401, description = "Not signed in"),
        (status = 503, description = "Annual plan pricing is invalid")
    )
)]
pub async fn annual_upgrade_quote(
    State(state): State<AppState>,
    cookies: Cookies,
) -> AppResult<Json<AnnualUpgradeQuoteResponse>> {
    let user = session::optional_current_user(&cookies, &state)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Sign in to view upgrade pricing.".to_string()))?;
    let billing_state = get_billing_state(&state.db, &user.id).await?;
    let current_interval = billing_state
        .interval
        .map(|value| value.as_str().to_string());
    let current_expires_at = billing_state.expires_at.map(|value| value.to_rfc3339());

    if billing_state.status != BillingStatus::Active
        || billing_state.interval != Some(BillingInterval::Month)
    {
        return Ok(Json(AnnualUpgradeQuoteResponse {
            eligible: false,
            current_interval,
            current_expires_at,
            annual_price_minor: None,
            unused_credit_minor: None,
            amount_due_minor: None,
            currency: None,
        }));
    }

    let quote = quote_checkout(
        &state.config,
        BillingInterval::Year,
        &billing_state,
        Utc::now(),
    );
    if quote.blocked {
        return Err(AppError::Conflict(
            quote
                .blocked_message
                .unwrap_or_else(|| "Annual upgrade is unavailable.".to_string()),
        ));
    }

    let annual_price_minor = billing::paddle::checkout_amount_to_paddle_minor(
        billing::checkout_amount_minor(&state.config, BillingInterval::Year),
    )
    .ok_or_else(|| {
        AppError::ServiceUnavailable("Annual Pro price is not configured.".to_string())
    })?;
    let amount_due_minor = billing::paddle::checkout_amount_to_paddle_minor(quote.amount_minor)
        .ok_or_else(|| {
            AppError::ServiceUnavailable("Annual upgrade amount is invalid.".to_string())
        })?;
    let unused_credit_minor = annual_price_minor
        .checked_sub(amount_due_minor)
        .filter(|credit| *credit >= 0)
        .ok_or_else(|| {
            AppError::ServiceUnavailable("Annual upgrade quote is inconsistent.".to_string())
        })?;

    Ok(Json(AnnualUpgradeQuoteResponse {
        eligible: true,
        current_interval,
        current_expires_at,
        annual_price_minor: Some(annual_price_minor),
        unused_credit_minor: Some(unused_credit_minor),
        amount_due_minor: Some(amount_due_minor),
        currency: Some("USD".to_string()),
    }))
}

/// Create a payment checkout for the signed-in user to upgrade to Pro.
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
    let attempt = PaymentAttempt::new(&state.config.payment_gateway, "checkout_creation");
    let user = session::optional_current_user(&cookies, &state)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Sign in to upgrade to Pro.".to_string()))?;

    if let Some(plan) = body.plan.as_deref() {
        if plan != "lifetime" || body.interval.is_some() {
            return Err(AppError::BadRequest(
                "plan must be lifetime and cannot be combined with interval.".to_string(),
            ));
        }

        let billing_state = get_billing_state(&state.db, &user.id).await?;
        let quote = billing::quote_lifetime_checkout(&state.config, &billing_state, Utc::now());
        if quote.blocked {
            return Err(AppError::Conflict(
                quote
                    .blocked_message
                    .unwrap_or_else(|| "Lifetime Pro is already active.".to_string()),
            ));
        }

        if state.config.payment_gateway == "paddle" {
            let checkout = billing::paddle::create_lifetime_transaction(
                &state.config,
                quote.amount_minor,
                quote.credit_minor,
                &user.id,
            )
            .await?;
            attempt.succeed();
            return Ok(Json(CreateOrderResponse {
                gateway: "paddle".to_string(),
                id: None,
                amount: None,
                currency: None,
                key: None,
                transaction_id: Some(checkout.transaction_id),
                client_token: Some(checkout.client_token),
                environment: Some(checkout.environment),
            }));
        }
        if state.config.payment_gateway != "razorpay" {
            return Err(AppError::ServiceUnavailable(format!(
                "Unsupported payment gateway '{}'.",
                state.config.payment_gateway
            )));
        }
        if quote.amount_minor < billing::RAZORPAY_MIN_AMOUNT {
            return Err(AppError::BadRequest(
                "Amount must be at least 100 (smallest currency unit).".to_string(),
            ));
        }
        let key = razorpay::checkout_key_id(&state.config)
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| {
                AppError::ServiceUnavailable("RAZORPAY_KEY_ID is not configured.".to_string())
            })?;
        let mut notes = HashMap::new();
        notes.insert("sketchflowUserId".to_string(), user.id.clone());
        notes.insert("planInterval".to_string(), "lifetime".to_string());
        notes.insert(
            "expectedAmountMinor".to_string(),
            quote.amount_minor.to_string(),
        );
        notes.insert("creditMinor".to_string(), quote.credit_minor.to_string());
        let now = Utc::now();
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
        attempt.succeed();
        return Ok(Json(CreateOrderResponse {
            gateway: "razorpay".to_string(),
            id: Some(order.id),
            amount: Some(order.amount),
            currency: Some(order.currency),
            key: Some(key),
            transaction_id: None,
            client_token: None,
            environment: None,
        }));
    }

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

    if state.config.payment_gateway == "paddle" {
        let switching_from_monthly = interval == BillingInterval::Year
            && billing_state.interval == Some(BillingInterval::Month);
        let checkout = billing::paddle::create_transaction(
            &state.config,
            quote.amount_minor,
            quote.interval,
            &user.id,
            switching_from_monthly,
        )
        .await?;
        attempt.succeed();
        return Ok(Json(CreateOrderResponse {
            gateway: "paddle".to_string(),
            id: None,
            amount: None,
            currency: None,
            key: None,
            transaction_id: Some(checkout.transaction_id),
            client_token: Some(checkout.client_token),
            environment: Some(checkout.environment),
        }));
    }
    if state.config.payment_gateway != "razorpay" {
        return Err(AppError::ServiceUnavailable(format!(
            "Unsupported payment gateway '{}'.",
            state.config.payment_gateway
        )));
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

    attempt.succeed();
    Ok(Json(CreateOrderResponse {
        gateway: "razorpay".to_string(),
        id: Some(order.id),
        amount: Some(order.amount),
        currency: Some(order.currency),
        key: Some(key),
        transaction_id: None,
        client_token: None,
        environment: None,
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

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct PaddleVerifyPaymentRequest {
    #[serde(rename = "transactionId")]
    pub transaction_id: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PaddleWebhookResponse {
    pub received: bool,
}

/// Verify a completed Paddle transaction and activate the Pro subscription.
#[utoipa::path(
    post,
    path = "/api/paddle/verify-payment",
    tag = "billing",
    request_body = PaddleVerifyPaymentRequest,
    responses(
        (status = 200, description = "Payment verified and plan activated", body = billing::paddle::PaddleCompletion),
        (status = 400, description = "Invalid transaction or payment state"),
        (status = 401, description = "Not signed in"),
        (status = 403, description = "Payment belongs to a different user")
    )
)]
pub async fn verify_paddle_payment(
    State(state): State<AppState>,
    cookies: Cookies,
    AppJson(body): AppJson<PaddleVerifyPaymentRequest>,
) -> AppResult<Json<billing::paddle::PaddleCompletion>> {
    let attempt = PaymentAttempt::new("paddle", "payment_verification");
    let user = session::optional_current_user(&cookies, &state)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Sign in to verify payment.".to_string()))?;
    let transaction_id = body
        .transaction_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::BadRequest("transactionId is required.".to_string()))?;

    let completion = billing::paddle::complete_transaction(
        &state.db,
        &state.config,
        transaction_id,
        Some(&user.id),
    )
    .await?;
    attempt.succeed();
    Ok(Json(completion))
}

/// Receive Paddle transaction events after validating the signature over the raw body.
#[utoipa::path(
    post,
    path = "/api/paddle/webhook",
    tag = "billing",
    params(
        ("Paddle-Signature" = String, Header, description = "Paddle webhook signature")
    ),
    request_body(content = String, content_type = "application/json"),
    responses(
        (status = 200, description = "Webhook acknowledged", body = PaddleWebhookResponse),
        (status = 400, description = "Invalid webhook payload"),
        (status = 403, description = "Invalid webhook signature"),
        (status = 503, description = "Webhook secret not configured")
    )
)]
pub async fn paddle_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw_body: Bytes,
) -> AppResult<Json<PaddleWebhookResponse>> {
    let mut attempt = PaymentAttempt::new("paddle", "webhook_processing");
    let signature = headers
        .get("Paddle-Signature")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Paddle-Signature header is required.".into()))?;
    let secret = billing::paddle::webhook_secret(&state.config)?;
    if !billing::paddle::webhook_signature_matches(&raw_body, signature, secret) {
        return Err(AppError::Forbidden(
            "Invalid Paddle webhook signature.".to_string(),
        ));
    }

    let event: Value = serde_json::from_slice(&raw_body)
        .map_err(|_| AppError::BadRequest("Paddle webhook payload is invalid.".into()))?;
    let event_type = event.get("event_type").and_then(Value::as_str);
    if event_type == Some("transaction.completed") {
        attempt.set_event_type("transaction.completed");
    } else if matches!(
        event_type,
        Some("adjustment.created") | Some("adjustment.updated")
    ) {
        attempt.set_event_type("adjustment");
    } else if event_type.is_some() {
        attempt.set_event_type("other");
    }
    if event_type == Some("transaction.completed") {
        let transaction_id = event
            .get("data")
            .and_then(|data| data.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                AppError::BadRequest("Paddle webhook transaction ID is missing.".into())
            })?;
        billing::paddle::complete_transaction(&state.db, &state.config, transaction_id, None)
            .await?;
    } else if matches!(
        event_type,
        Some("adjustment.created") | Some("adjustment.updated")
    ) {
        let data = event.get("data");
        let action = data
            .and_then(|data| data.get("action"))
            .and_then(Value::as_str);
        let status = data
            .and_then(|data| data.get("status"))
            .and_then(Value::as_str);
        let transaction_id = data
            .and_then(|data| data.get("transaction_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty());
        if let (Some(action), Some(transaction_id)) = (action, transaction_id) {
            let revoke_status = if action == "chargeback" {
                Some("chargeback")
            } else if action == "refund" && status == Some("approved") {
                Some("refunded")
            } else {
                None
            };
            if let Some(revoke_status) = revoke_status {
                billing::revoke_lifetime_purchase(&state.db, transaction_id, revoke_status).await?;
            }
        }
    }

    attempt.succeed();
    Ok(Json(PaddleWebhookResponse { received: true }))
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
    let attempt = PaymentAttempt::new("razorpay", "payment_verification");
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

    if order.notes.get("planInterval").map(String::as_str) == Some("lifetime") {
        let expected_amount = order
            .notes
            .get("expectedAmountMinor")
            .and_then(|value| value.parse::<i64>().ok());
        let credit_minor = order
            .notes
            .get("creditMinor")
            .and_then(|value| value.parse::<i64>().ok());
        let full_amount = state.config.plan_price_lifetime_minor;
        if order.notes.get("sketchflowUserId") != Some(&user.id)
            || expected_amount != Some(payment.amount)
            || order.amount != payment.amount
            || payment.currency.trim().to_uppercase()
                != state.config.checkout_currency.trim().to_uppercase()
            || expected_amount
                .is_none_or(|amount| amount < billing::RAZORPAY_MIN_AMOUNT || amount > full_amount)
            || credit_minor
                .and_then(|credit| expected_amount.and_then(|amount| amount.checked_add(credit)))
                != Some(full_amount)
        {
            return Err(AppError::BadRequest(
                "Razorpay Lifetime Pro order amount or currency is invalid.".to_string(),
            ));
        }

        let activated = billing::activate_lifetime_purchase(
            &state.db,
            &user.id,
            "razorpay",
            order_id,
            payment_id,
            payment.amount,
            credit_minor.unwrap_or_default(),
            &payment.currency,
        )
        .await?;
        attempt.succeed();
        return Ok(Json(VerifyPaymentResponse {
            success: true,
            status: payment.status,
            plan: "pro".to_string(),
            interval: "lifetime".to_string(),
            expires_at: activated
                .expires_at
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }));
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

    attempt.succeed();
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
