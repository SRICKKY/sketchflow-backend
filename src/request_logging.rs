use std::{
    env,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

// Tee request data as it streams through Axum; the logger never buffers the full body.
use axum::{
    body::Body,
    extract::MatchedPath,
    http::{
        header::{self, HeaderName, HeaderValue},
        Request,
    },
    middleware::Next,
    response::Response,
};
use http_body_util::BodyExt;
use serde_json::Value;
use tracing::Instrument;

const MAX_LOGGED_JSON_BYTES: usize = 16 * 1024;
const REDACTED: &str = "[REDACTED]";
const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

#[derive(Clone, Debug)]
pub struct RequestId(pub String);

static APP_ENVIRONMENT: OnceLock<String> = OnceLock::new();

pub fn app_environment() -> &'static str {
    APP_ENVIRONMENT
        .get_or_init(|| {
            env::var("APP_ENV")
                .ok()
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "development".to_owned())
        })
        .as_str()
}

pub fn generate_request_id() -> String {
    format!("req_{}", cuid2::create_id())
}

#[derive(Default)]
struct BodyCapture {
    bytes: Vec<u8>,
    observed_bytes: usize,
    truncated: bool,
}

impl BodyCapture {
    fn record(&mut self, data: &[u8], capture_payload: bool) {
        self.observed_bytes = self.observed_bytes.saturating_add(data.len());
        if !capture_payload {
            return;
        }

        let remaining = MAX_LOGGED_JSON_BYTES.saturating_sub(self.bytes.len());
        let copied = remaining.min(data.len());
        self.bytes.extend_from_slice(&data[..copied]);
        self.truncated |= copied < data.len();
    }
}

pub async fn log_request_payload(request: Request<Body>, next: Next) -> Response {
    let (mut parts, body) = request.into_parts();
    let method = parts.method.clone();
    let path = parts
        .extensions
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_owned())
        .unwrap_or_else(|| parts.uri.path().to_owned());
    let request_id = parts
        .headers
        .get(&REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| is_valid_request_id(value))
        .map(str::to_owned)
        .unwrap_or_else(generate_request_id);
    parts.extensions.insert(RequestId(request_id.clone()));
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let is_json = content_type.as_deref().is_some_and(is_json_content_type);
    let content_length = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    let capture = Arc::new(Mutex::new(BodyCapture::default()));
    let body_capture = Arc::clone(&capture);
    let tracked_body = body.map_frame(move |frame| {
        if let Some(data) = frame.data_ref() {
            body_capture
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .record(data.as_ref(), is_json);
        }
        frame
    });

    let span = tracing::info_span!(
        "http_request",
        service = "sketchflow-api",
        environment = app_environment(),
        request_id = %request_id,
        method = %method,
        endpoint = %path,
    );
    async move {
        let started = Instant::now();
        let mut response = next
            .run(Request::from_parts(parts, Body::new(tracked_body)))
            .await;
        let capture = capture
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let payload = format_payload(&capture, is_json, content_length, content_type.as_deref());

        tracing::info!(
            service = "sketchflow-api",
            environment = app_environment(),
            request_id = %request_id,
            subsystem = "http",
            event = "request_completed",
            method = %method,
            endpoint = %path,
            status = response.status().as_u16(),
            duration_ms = started.elapsed().as_millis() as u64,
            body_bytes_observed = capture.observed_bytes,
            request_payload = %payload,
            "HTTP request completed"
        );

        response.headers_mut().insert(
            &REQUEST_ID_HEADER,
            HeaderValue::from_str(&request_id).expect("validated request IDs are valid headers"),
        );
        response
    }
    .instrument(span)
    .await
}

fn is_valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn is_json_content_type(content_type: &str) -> bool {
    let media_type = content_type.split(';').next().unwrap_or_default().trim();

    media_type.eq_ignore_ascii_case("application/json")
        || media_type
            .rsplit_once('/')
            .is_some_and(|(_, subtype)| subtype.to_ascii_lowercase().ends_with("+json"))
}

fn format_payload(
    capture: &BodyCapture,
    is_json: bool,
    content_length: Option<u64>,
    content_type: Option<&str>,
) -> String {
    if capture.observed_bytes == 0 {
        return if content_length.is_some_and(|length| length > 0) {
            "<body not consumed>".to_owned()
        } else {
            "<empty>".to_owned()
        };
    }
    if !is_json {
        return format!(
            "<omitted: non-JSON body; content-type={}>",
            content_type.unwrap_or("unknown")
        );
    }
    if capture.truncated {
        return format!("<omitted: JSON body exceeds {MAX_LOGGED_JSON_BYTES} byte logging limit>");
    }
    if content_length.is_some_and(|length| length > capture.observed_bytes as u64) {
        return "<omitted: JSON body was not fully consumed>".to_owned();
    }

    match serde_json::from_slice::<Value>(&capture.bytes) {
        Ok(mut value) => {
            redact_sensitive_fields(&mut value);
            match serde_json::to_string(&value) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(
                        subsystem = "http",
                        event = "request_payload_serialization_failed",
                        %error,
                        "Failed to serialize redacted API request payload"
                    );
                    "<JSON serialization failed>".to_owned()
                }
            }
        }
        Err(_) => "<omitted: invalid or incomplete JSON body>".to_owned(),
    }
}

fn redact_sensitive_fields(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if is_sensitive_key(key) {
                    *value = Value::String(REDACTED.to_owned());
                } else {
                    redact_sensitive_fields(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_sensitive_fields(value);
            }
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();

    [
        "password",
        "passphrase",
        "token",
        "secret",
        "apikey",
        "session",
        "authorization",
        "cookie",
        "signature",
        "email",
        "phone",
        "address",
        "name",
        "card",
        "cvv",
        "cvc",
        "otp",
        "privatekey",
        "credential",
        "prompt",
        "content",
        "description",
        "message",
    ]
    .iter()
    .any(|sensitive_fragment| normalized.contains(sensitive_fragment))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        middleware,
        routing::post,
        Json, Router,
    };
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::{format_payload, log_request_payload, redact_sensitive_fields, BodyCapture};

    #[test]
    fn redacts_sensitive_fields_recursively() {
        let mut payload = json!({
            "email": "person@example.com",
            "profile": { "displayName": "Private Name" },
            "password": "do-not-log",
            "access_token": "do-not-log",
            "items": [{ "message": "private text", "count": 2 }]
        });

        redact_sensitive_fields(&mut payload);

        assert_eq!(
            payload,
            json!({
                "email": "[REDACTED]",
                "profile": { "displayName": "[REDACTED]" },
                "password": "[REDACTED]",
                "access_token": "[REDACTED]",
                "items": [{ "message": "[REDACTED]", "count": 2 }]
            })
        );
    }

    #[test]
    fn formatted_payload_redacts_before_logging() {
        let body = br#"{"password":"do-not-log","count":2}"#;
        let mut capture = BodyCapture::default();
        capture.record(body, true);

        let logged: Value = serde_json::from_str(&format_payload(
            &capture,
            true,
            Some(body.len() as u64),
            Some("application/json"),
        ))
        .unwrap();

        assert_eq!(logged["password"], "[REDACTED]");
        assert_eq!(logged["count"], 2);
    }

    #[test]
    fn omits_large_json_bodies_and_non_json_payloads() {
        let mut capture = BodyCapture::default();
        capture.record(&vec![b'x'; super::MAX_LOGGED_JSON_BYTES + 1], true);
        assert!(
            format_payload(&capture, true, None, Some("application/json"))
                .contains("logging limit")
        );

        let mut capture = BodyCapture::default();
        capture.record(b"binary", false);
        assert!(format_payload(&capture, false, None, Some("image/png")).contains("non-JSON body"));
    }

    #[tokio::test]
    async fn middleware_preserves_large_json_requests() {
        async fn accept_json(Json(payload): Json<Value>) -> StatusCode {
            assert_eq!(payload["data"].as_str().map(str::len), Some(20 * 1024));
            StatusCode::NO_CONTENT
        }

        let app = Router::new()
            .route("/api/test", post(accept_json))
            .layer(middleware::from_fn(log_request_payload));
        let body = serde_json::to_vec(&json!({ "data": "x".repeat(20 * 1024) })).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/test")
                    .header("content-type", "application/json")
                    .header("x-request-id", "req_test123")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response.headers().get("x-request-id").unwrap(),
            "req_test123"
        );
        assert!(to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn middleware_replaces_invalid_request_ids() {
        let app = Router::new()
            .route("/api/test", post(|| async { StatusCode::NO_CONTENT }))
            .layer(middleware::from_fn(log_request_payload));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/test")
                    .header("x-request-id", "not a valid id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let request_id = response
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(request_id.starts_with("req_"));
        assert!(super::is_valid_request_id(request_id));
    }
}
