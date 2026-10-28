use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use sqlx::PgPool;
use subtle::ConstantTimeEq;

use crate::{config::AppConfig, error::AppError};

use super::{activate_pro_subscription, BillingInterval};

const SANDBOX_API: &str = "https://sandbox-api.paddle.com";
const PRODUCTION_API: &str = "https://api.paddle.com";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PaddleCheckout {
    #[serde(rename = "transactionId")]
    pub transaction_id: String,
    #[serde(rename = "clientToken")]
    pub client_token: String,
    pub environment: String,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PaddleCompletion {
    pub success: bool,
    pub status: String,
    pub plan: String,
    pub interval: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
}

#[derive(Debug, Deserialize)]
struct PaddlePrice {
    id: String,
    billing_cycle: Value,
    unit_price: PaddleUnitPrice,
}

#[derive(Debug, Deserialize)]
struct PaddleUnitPrice {
    amount: String,
    currency_code: String,
}

#[derive(Debug, Deserialize)]
struct PaddleTransaction {
    id: String,
    status: String,
    currency_code: String,
    #[serde(default)]
    custom_data: Option<Value>,
    items: Vec<PaddleTransactionItem>,
    details: PaddleTransactionDetails,
}

#[derive(Debug, Deserialize)]
struct PaddleTransactionItem {
    #[serde(default)]
    price_id: Option<String>,
    #[serde(default)]
    price: Option<PaddleTransactionPrice>,
    quantity: i64,
}

#[derive(Debug, Deserialize)]
struct PaddleTransactionPrice {
    id: String,
}

impl PaddleTransactionItem {
    fn price_id(&self) -> Option<&str> {
        self.price
            .as_ref()
            .map(|price| price.id.as_str())
            .or(self.price_id.as_deref())
    }
}

#[derive(Debug, Deserialize)]
struct PaddleTransactionDetails {
    totals: Value,
}

fn api_base(config: &AppConfig) -> Result<&'static str, AppError> {
    match config.paddle_environment.as_str() {
        "sandbox" => Ok(SANDBOX_API),
        "production" => Ok(PRODUCTION_API),
        _ => Err(AppError::ServiceUnavailable(
            "PADDLE_ENV must be sandbox or production.".to_string(),
        )),
    }
}

fn required_config<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, AppError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::ServiceUnavailable(format!("{name} is not configured.")))
}

fn configured_price_id(config: &AppConfig, interval: BillingInterval) -> Result<&str, AppError> {
    let (value, name) = match interval {
        BillingInterval::Month => (
            config.paddle_price_id_monthly.as_deref(),
            "PADDLE_PRICE_ID_MONTHLY",
        ),
        BillingInterval::Year => (
            config.paddle_price_id_yearly.as_deref(),
            "PADDLE_PRICE_ID_YEARLY",
        ),
    };
    let id = required_config(value, name)?;
    if !valid_path_id(id) {
        return Err(AppError::ServiceUnavailable(format!("{name} is invalid.")));
    }
    Ok(id)
}

fn valid_path_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn expected_catalog_amount_minor(config: &AppConfig, interval: BillingInterval) -> i64 {
    let inr_minor = match interval {
        BillingInterval::Month => config.plan_price_month_minor,
        BillingInterval::Year => config.plan_price_year_minor,
    };
    ((i128::from(inr_minor) + 50) / 100).clamp(0, i128::from(i64::MAX)) as i64
}

fn checkout_amount_to_paddle_minor(amount_minor: i64) -> Option<i64> {
    (amount_minor > 0)
        .then(|| (i128::from(amount_minor) + 50) / 100)
        .and_then(|amount| i64::try_from(amount).ok())
}

fn paddle_client() -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("failed to create HTTP client")))
}

async fn paddle_request(
    config: &AppConfig,
    path: &str,
    method: reqwest::Method,
    body: Option<Value>,
    idempotency_key: Option<&str>,
) -> Result<Value, AppError> {
    let base = api_base(config)?;
    let api_key = required_config(config.paddle_api_key.as_deref(), "PADDLE_API_KEY")?;
    let client = paddle_client()?;
    let mut request = client
        .request(method, format!("{base}{path}"))
        .bearer_auth(api_key)
        .header(reqwest::header::ACCEPT, "application/json");
    if let Some(body) = body {
        request = request.json(&body);
    }
    if let Some(key) = idempotency_key {
        request = request.header("Idempotency-Key", key);
    }
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            AppError::Gateway(504, "Paddle timed out.".to_string())
        } else {
            AppError::Gateway(503, "Paddle is unreachable.".to_string())
        }
    })?;
    let status = response.status();
    let value = response
        .json::<Value>()
        .await
        .map_err(|_| AppError::Gateway(502, "Paddle returned an invalid response.".to_string()))?;
    if !status.is_success() {
        let message = value
            .get("error")
            .and_then(|error| error.get("detail").or_else(|| error.get("message")))
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Paddle error ({status})."));
        let mapped_status = if status.as_u16() == 401 || status.as_u16() == 403 {
            503
        } else if status.as_u16() == 429 || status.is_server_error() {
            502
        } else {
            400
        };
        return Err(AppError::Gateway(mapped_status, message));
    }
    Ok(value)
}

async fn validate_price(
    config: &AppConfig,
    interval: BillingInterval,
    price_id: &str,
) -> Result<(), AppError> {
    let value = paddle_request(
        config,
        &format!("/prices/{price_id}"),
        reqwest::Method::GET,
        None,
        None,
    )
    .await?;
    let price: PaddlePrice = serde_json::from_value(
        value
            .get("data")
            .cloned()
            .ok_or_else(|| AppError::Gateway(502, "Paddle price response is invalid.".into()))?,
    )
    .map_err(|_| AppError::Gateway(502, "Paddle price response is invalid.".into()))?;
    let expected = expected_catalog_amount_minor(config, interval);
    let actual = price.unit_price.amount.parse::<i64>().ok();
    if price.id != price_id
        || !price.billing_cycle.is_null()
        || price.unit_price.currency_code != "USD"
        || actual != Some(expected)
    {
        return Err(AppError::ServiceUnavailable(format!(
            "Paddle {} price must be a one-time USD price for the configured plan amount.",
            interval.as_str()
        )));
    }
    Ok(())
}

pub async fn create_transaction(
    config: &AppConfig,
    amount_minor: i64,
    interval: BillingInterval,
    user_id: &str,
    switching_from_monthly: bool,
) -> Result<PaddleCheckout, AppError> {
    let environment = config.paddle_environment.as_str();
    let client_token =
        required_config(config.paddle_client_token.as_deref(), "PADDLE_CLIENT_TOKEN")?;
    let price_id = configured_price_id(config, interval)?;
    let expected_amount = checkout_amount_to_paddle_minor(amount_minor)
        .ok_or_else(|| AppError::BadRequest("Paddle checkout amount is invalid.".into()))?;
    let catalog_amount = expected_catalog_amount_minor(config, interval);
    if expected_amount > catalog_amount
        || (expected_amount != catalog_amount
            && !(interval == BillingInterval::Year && switching_from_monthly))
    {
        return Err(AppError::BadRequest(
            "Paddle checkout amount is invalid.".to_string(),
        ));
    }
    validate_price(config, interval, price_id).await?;

    let discount = catalog_amount - expected_amount;
    let mut body = json!({
        "collection_mode": "automatic",
        "currency_code": "USD",
        "items": [{ "price_id": price_id, "quantity": 1 }],
        "custom_data": {
            "sketchflow_user_id": user_id,
            "sketchflow_interval": interval.as_str(),
            "sketchflow_expected_amount_minor": expected_amount.to_string(),
            "sketchflow_switching_from_monthly": switching_from_monthly
        }
    });
    if discount > 0 {
        body["discount"] = json!({
            "description": "Unused monthly plan credit",
            "type": "flat",
            "amount": discount.to_string(),
            "recur": false
        });
    }

    let idempotency_key = cuid2::create_id();
    let value = paddle_request(
        config,
        "/transactions",
        reqwest::Method::POST,
        Some(body),
        Some(&idempotency_key),
    )
    .await?;
    let transaction_id = value
        .get("data")
        .and_then(|data| data.get("id"))
        .and_then(Value::as_str)
        .filter(|id| id.starts_with("txn_") && valid_path_id(id))
        .ok_or_else(|| {
            AppError::Gateway(502, "Paddle returned an invalid transaction ID.".into())
        })?;

    Ok(PaddleCheckout {
        transaction_id: transaction_id.to_string(),
        client_token: client_token.to_string(),
        environment: environment.to_string(),
    })
}

fn parse_interval(value: Option<&Value>) -> Option<BillingInterval> {
    match value.and_then(Value::as_str) {
        Some("month") => Some(BillingInterval::Month),
        Some("year") => Some(BillingInterval::Year),
        _ => None,
    }
}

fn parse_minor_amount(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(amount) => amount.as_i64().filter(|amount| *amount >= 0),
        Value::String(amount)
            if !amount.is_empty() && amount.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            amount.parse::<i64>().ok()
        }
        _ => None,
    }
}

async fn fetch_transaction(
    config: &AppConfig,
    transaction_id: &str,
) -> Result<PaddleTransaction, AppError> {
    if !valid_path_id(transaction_id) {
        return Err(AppError::BadRequest(
            "transactionId is invalid.".to_string(),
        ));
    }
    let value = paddle_request(
        config,
        &format!("/transactions/{transaction_id}"),
        reqwest::Method::GET,
        None,
        None,
    )
    .await?;
    serde_json::from_value(
        value.get("data").cloned().ok_or_else(|| {
            AppError::Gateway(502, "Paddle transaction response is invalid.".into())
        })?,
    )
    .map_err(|_| AppError::Gateway(502, "Paddle transaction response is invalid.".into()))
}

fn transaction_custom_data(
    transaction: &PaddleTransaction,
) -> Result<(String, BillingInterval, i64, bool), AppError> {
    let custom_data = transaction
        .custom_data
        .as_ref()
        .and_then(Value::as_object)
        .ok_or_else(|| {
            AppError::BadRequest("Paddle transaction is missing valid plan details.".into())
        })?;
    let user_id = custom_data
        .get("sketchflow_user_id")
        .and_then(Value::as_str)
        .filter(|user_id| !user_id.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("Paddle transaction is missing valid plan details.".into())
        })?;
    let interval = parse_interval(custom_data.get("sketchflow_interval")).ok_or_else(|| {
        AppError::BadRequest("Paddle transaction is missing valid plan details.".into())
    })?;
    let expected_amount = parse_minor_amount(custom_data.get("sketchflow_expected_amount_minor"))
        .ok_or_else(|| {
        AppError::BadRequest("Paddle transaction plan amount is invalid.".into())
    })?;
    let switching_from_monthly = custom_data
        .get("sketchflow_switching_from_monthly")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((
        user_id.to_string(),
        interval,
        expected_amount,
        switching_from_monthly,
    ))
}

async fn verify_transaction(
    config: &AppConfig,
    transaction_id: &str,
    expected_user_id: Option<&str>,
) -> Result<(PaddleTransaction, String, BillingInterval, i64), AppError> {
    let transaction = fetch_transaction(config, transaction_id).await?;
    if transaction.id != transaction_id {
        return Err(AppError::BadRequest(
            "Paddle transaction ID does not match.".to_string(),
        ));
    }
    if transaction.status != "completed" && transaction.status != "paid" {
        return Err(AppError::Conflict(
            "Paddle transaction is not paid yet.".to_string(),
        ));
    }

    let (user_id, interval, expected_amount, switching_from_monthly) =
        transaction_custom_data(&transaction)?;
    if expected_user_id.is_some_and(|expected| expected != user_id) {
        return Err(AppError::Forbidden(
            "Paddle transaction does not belong to this user.".to_string(),
        ));
    }
    if transaction.currency_code.trim().to_uppercase() != "USD" {
        return Err(AppError::BadRequest(
            "Paddle transaction plan, amount, or currency is invalid.".to_string(),
        ));
    }

    let price_id = configured_price_id(config, interval)?;
    let catalog_amount = expected_catalog_amount_minor(config, interval);
    if transaction.items.len() != 1
        || transaction.items[0].price_id() != Some(price_id)
        || transaction.items[0].quantity != 1
        || expected_amount <= 0
        || (expected_amount != catalog_amount
            && !(interval == BillingInterval::Year
                && switching_from_monthly
                && expected_amount < catalog_amount))
    {
        return Err(AppError::BadRequest(
            "Paddle transaction plan, amount, or currency is invalid.".to_string(),
        ));
    }

    let totals = transaction
        .details
        .totals
        .as_object()
        .ok_or_else(|| AppError::BadRequest("Paddle transaction totals are invalid.".into()))?;
    let subtotal = parse_minor_amount(totals.get("subtotal"))
        .ok_or_else(|| AppError::BadRequest("Paddle transaction totals are invalid.".into()))?;
    let discount = parse_minor_amount(totals.get("discount"))
        .ok_or_else(|| AppError::BadRequest("Paddle transaction totals are invalid.".into()))?;
    let paid = parse_minor_amount(totals.get("grand_total").or_else(|| totals.get("total")))
        .ok_or_else(|| AppError::BadRequest("Paddle transaction totals are invalid.".into()))?;
    if subtotal != catalog_amount
        || subtotal.saturating_sub(discount) != expected_amount
        || paid < expected_amount
    {
        return Err(AppError::BadRequest(
            "Paddle transaction amount does not match the checkout.".to_string(),
        ));
    }

    Ok((transaction, user_id, interval, paid))
}

pub async fn complete_transaction(
    db: &PgPool,
    config: &AppConfig,
    transaction_id: &str,
    expected_user_id: Option<&str>,
) -> Result<PaddleCompletion, AppError> {
    let (transaction, user_id, interval, amount_minor) =
        verify_transaction(config, transaction_id, expected_user_id).await?;
    let activated = activate_pro_subscription(
        db,
        &user_id,
        interval,
        &format!("paddle_{transaction_id}"),
        transaction_id,
        transaction_id,
        amount_minor,
        "USD",
    )
    .await?;

    Ok(PaddleCompletion {
        success: true,
        status: transaction.status,
        plan: "pro".to_string(),
        interval: activated.interval.as_str().to_string(),
        expires_at: activated
            .expires_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

pub fn webhook_secret(config: &AppConfig) -> Result<&str, AppError> {
    required_config(
        config.paddle_webhook_secret.as_deref(),
        "PADDLE_WEBHOOK_SECRET",
    )
}

pub fn webhook_signature_matches(raw_body: &[u8], header: &str, secret: &str) -> bool {
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for component in header.split(';').map(str::trim) {
        let Some((key, value)) = component.split_once('=') else {
            continue;
        };
        match key {
            "ts" if value.bytes().all(|byte| byte.is_ascii_digit()) => timestamp = Some(value),
            "h1" => signatures.push(value),
            _ => {}
        }
    }
    let Some(timestamp) = timestamp else {
        return false;
    };
    if signatures.is_empty() {
        return false;
    }

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(raw_body);
    let expected = mac.finalize().into_bytes();
    signatures.iter().any(|signature| {
        let Ok(received) = hex::decode(signature) else {
            return false;
        };
        received.len() == expected.len()
            && bool::from(expected.as_slice().ct_eq(received.as_slice()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_inr_paise_to_usd_cents_using_app_checkout_rate() {
        assert_eq!(checkout_amount_to_paddle_minor(50_000), Some(500));
        assert_eq!(checkout_amount_to_paddle_minor(500_000), Some(5_000));
        assert_eq!(checkout_amount_to_paddle_minor(0), None);
    }

    #[test]
    fn parses_paddle_transaction_items_with_nested_price() {
        let transaction: PaddleTransaction = serde_json::from_value(json!({
            "id": "txn_01h00000000000000000000001",
            "status": "completed",
            "currency_code": "USD",
            "items": [{
                "price": { "id": "pri_01h00000000000000000000001" },
                "quantity": 1
            }],
            "details": { "totals": {} }
        }))
        .unwrap();

        assert_eq!(
            transaction.items[0].price_id(),
            Some("pri_01h00000000000000000000001")
        );
    }

    #[test]
    fn verifies_paddle_signature_and_rejects_tampering() {
        let body = br#"{"event_type":"transaction.completed"}"#;
        let timestamp = "1760000000";
        let secret = "webhook-secret";
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(timestamp.as_bytes());
        mac.update(b":");
        mac.update(body);
        let signature = hex::encode(mac.finalize().into_bytes());

        assert!(webhook_signature_matches(
            body,
            &format!("ts={timestamp};h1={signature}"),
            secret
        ));
        assert!(!webhook_signature_matches(
            br#"{"event_type":"transaction.canceled"}"#,
            &format!("ts={timestamp};h1={signature}"),
            secret
        ));
        assert!(!webhook_signature_matches(
            body,
            "h1=not-a-signature",
            secret
        ));
    }

    #[test]
    fn rejects_path_ids_with_url_control_characters() {
        assert!(valid_path_id("txn_abc-123"));
        assert!(!valid_path_id("../txn_abc"));
        assert!(!valid_path_id(""));
    }
}
