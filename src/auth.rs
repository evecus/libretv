//! 会话鉴权：httpOnly cookie + HMAC-SHA256 签名，契约与原版 auth.ts 完全一致
//! （cookie 名 ltv_session、`<expiresMs>.<hmacHex>` 格式、恒定时间比较）。

use axum::http::{header, HeaderMap};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{config::Config, error::ApiError, state::AppState, util::now_ms};

type HmacSha256 = Hmac<Sha256>;

pub const SESSION_COOKIE: &str = "ltv_session";
const SESSION_TTL_MS: i64 = 90 * 24 * 60 * 60 * 1000;

fn hmac_hex(cfg: &Config, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(&cfg.session_secret).expect("HMAC key");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn sign_session(cfg: &Config) -> (String, i64) {
    let expires = now_ms() + SESSION_TTL_MS;
    let payload = expires.to_string();
    (format!("{payload}.{}", hmac_hex(cfg, &payload)), expires)
}

pub fn verify_session(cfg: &Config, token: Option<&str>) -> bool {
    let token = match token {
        Some(t) => t,
        None => return false,
    };
    let dot = match token.rfind('.') {
        Some(d) if d > 0 => d,
        _ => return false,
    };
    let payload = &token[..dot];
    let sig = &token[dot + 1..];
    let expected = hmac_hex(cfg, payload);
    if sig.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() != 1 {
        return false;
    }
    match payload.parse::<i64>() {
        Ok(expires) => now_ms() < expires,
        Err(_) => false,
    }
}

/// 恒定时间比较密码（先各自 sha256，避免长度泄漏）
pub fn check_password(cfg: &Config, input: &str) -> bool {
    if !cfg.password_configured() {
        return false;
    }
    let a = Sha256::digest(input.as_bytes());
    let b = Sha256::digest(cfg.password.as_bytes());
    a.ct_eq(&b).unwrap_u8() == 1
}

pub fn session_from_cookie(cfg: &Config, headers: &HeaderMap) -> bool {
    let raw = match headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        Some(v) => v,
        None => return false,
    };
    for part in raw.split(';') {
        let (name, value) = match part.split_once('=') {
            Some((n, v)) => (n.trim(), v.trim()),
            None => continue,
        };
        if name == SESSION_COOKIE {
            let decoded = crate::util::decode_component(value);
            return verify_session(cfg, Some(&decoded));
        }
    }
    false
}

/// API 共享守卫：未配置密码 503、未登录 401。前端据此弹出登录框 / 提示管理员。
pub fn guard(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if !state.cfg.password_configured() {
        return Err(ApiError::ServiceUnavailable("服务器未设置 PASSWORD 环境变量".into()));
    }
    if !session_from_cookie(&state.cfg, headers) {
        return Err(ApiError::Unauthorized("未登录".into()));
    }
    Ok(())
}

pub fn session_cookie_header(state: &AppState, headers: &HeaderMap) -> (String, i64) {
    let (token, expires) = sign_session(&state.cfg);
    let secure = state
        .cfg
        .cookie_secure
        .unwrap_or_else(|| crate::util::request_is_https(headers));
    let mut value = format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        ((expires - now_ms()) / 1000).max(0)
    );
    if secure {
        value.push_str("; Secure");
    }
    (value, expires)
}

pub fn logout_cookie_header() -> &'static str {
    "ltv_session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"
}
