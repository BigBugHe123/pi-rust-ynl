use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::json;
use std::net::IpAddr;

use crate::web_backend::AppState;

const SESSION_COOKIE: &str = "pi_web_session";
const SESSION_MAX_AGE_SECONDS: u64 = 60 * 60 * 24 * 30;
const BASE_DELAY: Duration = Duration::from_millis(1_000);
const MAX_DELAY: Duration = Duration::from_millis(60_000);
const RESET_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
struct Throttle {
    failures: u32,
    last_failure: Option<Instant>,
    blocked_until: Option<Instant>,
}

fn throttle() -> &'static Mutex<Throttle> {
    static STATE: OnceLock<Mutex<Throttle>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Throttle::default()))
}

fn retry_after() -> Duration {
    let now = Instant::now();
    let Ok(mut state) = throttle().lock() else { return MAX_DELAY };
    if state.last_failure.is_some_and(|last| now.duration_since(last) >= RESET_AFTER) {
        *state = Throttle::default();
    }
    state.blocked_until.map_or(Duration::ZERO, |until| until.saturating_duration_since(now))
}

fn record_failure() -> Duration {
    let now = Instant::now();
    let Ok(mut state) = throttle().lock() else { return MAX_DELAY };
    if state.last_failure.is_some_and(|last| now.duration_since(last) >= RESET_AFTER) {
        *state = Throttle::default();
    }
    state.failures = state.failures.saturating_add(1);
    state.last_failure = Some(now);
    let exponent = state.failures.saturating_sub(1).min(31);
    let delay = BASE_DELAY.saturating_mul(1u32 << exponent).min(MAX_DELAY);
    state.blocked_until = Some(now + delay);
    delay
}

fn record_success() {
    if let Ok(mut state) = throttle().lock() {
        *state = Throttle::default();
    }
}

fn seconds_ceil(duration: Duration) -> u64 {
    duration.as_secs().saturating_add(u64::from(duration.subsec_nanos() > 0)).max(1)
}

fn limited(retry_after: Duration) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({
            "error": "Too many failed attempts",
            "retryAfterMs": retry_after.as_millis().min(u64::MAX as u128) as u64
        })),
    )
        .into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(value) = HeaderValue::from_str(&seconds_ceil(retry_after).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn password() -> Option<String> {
    std::env::var("PI_WEB_PASSWORD").ok().filter(|value| !value.is_empty())
}

fn secret_eq(actual: &str, expected: &str) -> bool {
    // HMAC verification compares the fixed-size tags in constant time.
    let key = hmac::Key::new(hmac::HMAC_SHA256, b"pi-web-secret-compare-v1");
    let expected_tag = hmac::sign(&key, expected.as_bytes());
    hmac::verify(&key, actual.as_bytes(), expected_tag.as_ref()).is_ok()
}

fn token_signature(payload: &str, password: &str) -> hmac::Tag {
    let key = hmac::Key::new(hmac::HMAC_SHA256, password.as_bytes());
    hmac::sign(&key, format!("pi-web-session:{payload}").as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    result
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            };
            Some((digit(pair[0])? << 4) | digit(pair[1])?)
        })
        .collect()
}

fn create_token(password: &str) -> Option<String> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let expires = now.checked_add(SESSION_MAX_AGE_SECONDS)?;
    let mut nonce = [0u8; 16];
    SystemRandom::new().fill(&mut nonce).ok()?;
    let payload = format!("v1.{expires}.{}", hex(&nonce));
    Some(format!("{payload}.{}", hex(token_signature(&payload, password).as_ref())))
}

fn valid_token(token: &str, password: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(version), Some(expires), Some(nonce), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if version != "v1"
        || nonce.len() != 32
        || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || signature.len() != 64
        || !signature.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return false;
    }
    if expires.is_empty() || !expires.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let Ok(expires) = expires.parse::<u64>() else { return false };
    if expires > 9_007_199_254_740_991 {
        return false;
    }
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else { return false };
    if expires <= now.as_secs() {
        return false;
    }
    let payload = format!("{version}.{expires}.{nonce}");
    let Some(signature_bytes) = decode_hex(signature) else { return false };
    hmac::verify(
        &hmac::Key::new(hmac::HMAC_SHA256, password.as_bytes()),
        format!("pi-web-session:{payload}").as_bytes(),
        &signature_bytes,
    )
    .is_ok()
}

fn valid_basic(authorization: Option<&HeaderValue>, password: &str) -> bool {
    let Some(value) = authorization.and_then(|value| value.to_str().ok()) else { return false };
    let Some((scheme, encoded)) = value.split_once(char::is_whitespace) else { return false };
    if !scheme.eq_ignore_ascii_case("Basic") {
        return false;
    }
    let encoded = encoded.trim_start();
    if encoded.is_empty() || encoded.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return false;
    }
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else { return false };
    if base64::engine::general_purpose::STANDARD.encode(&decoded) != encoded {
        return false;
    }
    let Ok(credentials) = std::str::from_utf8(&decoded) else { return false };
    let Some((username, supplied)) = credentials.split_once(':') else { return false };
    secret_eq(username, "pi") && secret_eq(supplied, password)
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| (name == SESSION_COOKIE).then(|| value.to_owned()))
}

fn is_api_basic(headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(char::is_whitespace))
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("Basic"))
}

fn auth_state(headers: &HeaderMap, throttle_basic: bool) -> Result<(bool, bool), Duration> {
    let Some(password) = password() else { return Ok((false, true)) };
    if cookie_value(headers).is_some_and(|token| valid_token(&token, &password)) {
        return Ok((true, true));
    }
    if is_api_basic(headers) {
        if throttle_basic {
            let delay = retry_after();
            if !delay.is_zero() {
                return Err(delay);
            }
        }
        let authenticated = valid_basic(headers.get(header::AUTHORIZATION), &password);
        if throttle_basic && !authenticated {
            record_failure();
        }
        return Ok((true, authenticated));
    }
    Ok((true, false))
}

pub async fn require_auth(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if !path.starts_with("/api/") {
        return next.run(request).await;
    }
    if path == "/api/v1/health"
        || path == "/api/v1/web-auth"
        || path == "/api/v1/web-auth/gate"
        || path == "/api/web-auth"
    {
        return next.run(request).await;
    }

    if std::env::var("PI_WEB_BACKEND_TRUST_LOOPBACK").is_ok_and(|value| value == "1")
        && request
            .extensions()
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .is_some_and(|ConnectInfo(address)| address.ip().is_loopback())
        && request
            .headers()
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            // Reverse proxies append the observed peer address after any
            // forwarded chain. Trust the rightmost address, never a client-
            // supplied leftmost value.
            .and_then(|value| value.rsplit(',').next())
            .is_none_or(|value| value.trim().parse::<IpAddr>().is_ok_and(|address| address.is_loopback()))
    {
        return next.run(request).await;
    }

    match auth_state(request.headers(), true) {
        Ok((_, true)) => next.run(request).await,
        Ok((_, false)) => {
            let mut response =
                (StatusCode::UNAUTHORIZED, Json(json!({"error":"Authentication required"}))).into_response();
            response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"Pi Web\", charset=\"UTF-8\""),
            );
            response
        }
        Err(delay) => limited(delay),
    }
}

pub async fn status(State(_state): State<AppState>, headers: HeaderMap) -> Response {
    match auth_state(&headers, false) {
        Ok((enabled, authenticated)) => {
            let mut response = Json(json!({ "enabled": enabled, "authenticated": authenticated })).into_response();
            response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(delay) => limited(delay),
    }
}

pub async fn gate(State(_state): State<AppState>, headers: HeaderMap) -> Response {
    match auth_state(&headers, true) {
        Ok((enabled, authenticated)) => {
            Json(json!({ "enabled": enabled, "authenticated": authenticated })).into_response()
        }
        Err(delay) => limited(delay),
    }
}

#[derive(Deserialize)]
pub struct LoginRequest {
    password: Option<String>,
}

pub async fn login(headers: HeaderMap, Json(body): Json<LoginRequest>) -> Response {
    let Some(expected) = password() else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "Password authentication is disabled" })))
            .into_response();
    };
    let delay = retry_after();
    if !delay.is_zero() {
        return limited(delay);
    }
    let supplied = body.password.as_deref().unwrap_or_default();
    if !secret_eq(supplied, &expected) {
        let delay = record_failure();
        let mut response = (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Invalid password", "retryAfterMs": delay.as_millis() as u64 })),
        )
            .into_response();
        if let Ok(value) = HeaderValue::from_str(&seconds_ceil(delay).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    record_success();
    match create_token(&expected) {
        Some(token) => {
            let mut response = Json(json!({ "ok": true })).into_response();
            let secure = headers.get("x-forwarded-proto").and_then(|value| value.to_str().ok()).is_some_and(|value| {
                value.split(',').next().is_some_and(|part| part.trim().eq_ignore_ascii_case("https"))
            });
            let cookie = format!(
                "{SESSION_COOKIE}={token}; Path=/; Max-Age={SESSION_MAX_AGE_SECONDS}; HttpOnly; SameSite=Lax{}",
                if secure { "; Secure" } else { "" }
            );
            if let Ok(value) = HeaderValue::from_str(&cookie) {
                response.headers_mut().insert(header::SET_COOKIE, value);
            }
            response
        }
        None => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "Could not create session" }))).into_response()
        }
    }
}

pub async fn logout() -> Response {
    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("pi_web_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax"),
    );
    response
}
