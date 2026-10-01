use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::{config::AppConfig, error::AppError};

const RAZORPAY_API: &str = "https://api.razorpay.com/v1";

#[derive(Debug, Deserialize, Serialize)]
pub struct RazorpayOrder {
    pub id: String,
    pub amount: i64,
    pub currency: String,
    #[serde(default)]
    pub notes: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RazorpayPayment {
    pub id: String,
    pub order_id: String,
    pub status: String,
    pub amount: i64,
    pub currency: String,
}

/// Returns the publishable key used by the client-side Razorpay checkout
/// widget. Falls back to `NEXT_PUBLIC_RAZORPAY_KEY_ID` is not applicable here
/// (that variable is Next.js specific), so this simply mirrors `key_id`.
pub fn checkout_key_id(config: &AppConfig) -> Option<String> {
    config.razorpay_key_id.clone()
}

fn credentials(config: &AppConfig) -> Result<(&str, &str), AppError> {
    let key_id = config
        .razorpay_key_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            AppError::ServiceUnavailable(
                "RAZORPAY_KEY_ID and RAZORPAY_KEY_SECRET are not configured".to_string(),
            )
        })?;
    let key_secret = config
        .razorpay_key_secret
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            AppError::ServiceUnavailable(
                "RAZORPAY_KEY_ID and RAZORPAY_KEY_SECRET are not configured".to_string(),
            )
        })?;
    Ok((key_id, key_secret))
}

/// `HMAC-SHA256("{order_id}|{payment_id}", key_secret)` as used by Razorpay
/// to sign successful checkout callbacks.
pub fn payment_signature(order_id: &str, payment_id: &str, key_secret: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key_secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("{order_id}|{payment_id}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time comparison of the expected vs. received signature.
pub fn signatures_match(expected: &str, received: &str) -> bool {
    expected.len() == received.len() && expected.as_bytes().ct_eq(received.as_bytes()).into()
}

pub fn is_captured_payment(status: &str) -> bool {
    status == "captured" || status == "authorized"
}

async fn read_error(response: reqwest::Response) -> AppError {
    let status = response.status();
    #[derive(Deserialize)]
    struct ErrBody {
        error: Option<ErrDetail>,
    }
    #[derive(Deserialize)]
    struct ErrDetail {
        description: Option<String>,
    }
    let message = response
        .json::<ErrBody>()
        .await
        .ok()
        .and_then(|b| b.error)
        .and_then(|e| e.description)
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| format!("Razorpay error ({status})"));
    let mapped_status = if status.as_u16() == 401 {
        503
    } else if status.as_u16() >= 500 {
        502
    } else {
        400
    };
    AppError::Gateway(mapped_status, message)
}

pub async fn create_order(
    config: &AppConfig,
    amount: i64,
    currency: &str,
    receipt: &str,
    notes: std::collections::HashMap<String, String>,
) -> Result<RazorpayOrder, AppError> {
    let (key_id, key_secret) = credentials(config)?;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "amount": amount,
        "currency": currency,
        "receipt": &receipt[..receipt.len().min(40)],
        "notes": notes,
    });
    let response = client
        .post(format!("{RAZORPAY_API}/orders"))
        .basic_auth(key_id, Some(key_secret))
        .json(&body)
        .send()
        .await
        .map_err(|_| AppError::Gateway(503, "Razorpay is unreachable".to_string()))?;
    if !response.status().is_success() {
        return Err(read_error(response).await);
    }
    response
        .json::<RazorpayOrder>()
        .await
        .map_err(|_| AppError::Gateway(502, "Razorpay returned an invalid response".to_string()))
}

pub async fn fetch_order(config: &AppConfig, order_id: &str) -> Result<RazorpayOrder, AppError> {
    let (key_id, key_secret) = credentials(config)?;
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{RAZORPAY_API}/orders/{order_id}"))
        .basic_auth(key_id, Some(key_secret))
        .send()
        .await
        .map_err(|_| AppError::Gateway(503, "Razorpay is unreachable".to_string()))?;
    if !response.status().is_success() {
        return Err(read_error(response).await);
    }
    response
        .json::<RazorpayOrder>()
        .await
        .map_err(|_| AppError::Gateway(502, "Razorpay returned an invalid response".to_string()))
}

pub async fn fetch_payment(
    config: &AppConfig,
    payment_id: &str,
) -> Result<RazorpayPayment, AppError> {
    let (key_id, key_secret) = credentials(config)?;
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{RAZORPAY_API}/payments/{payment_id}"))
        .basic_auth(key_id, Some(key_secret))
        .send()
        .await
        .map_err(|_| AppError::Gateway(503, "Razorpay is unreachable".to_string()))?;
    if !response.status().is_success() {
        return Err(read_error(response).await);
    }
    response
        .json::<RazorpayPayment>()
        .await
        .map_err(|_| AppError::Gateway(502, "Razorpay returned an invalid response".to_string()))
}
