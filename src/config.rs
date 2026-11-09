//! 运行时配置：环境变量契约与上游 Next.js 版本保持一致。
//! /api/status 会把这些值原样下发给前端设置面板。

use std::{path::PathBuf, sync::Arc, time::Duration};

use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Config {
    pub password: String,
    /// HMAC 密钥：PROXY_SECRET，或从 PASSWORD 派生（sha256 hex，与原版一致）
    pub session_secret: Vec<u8>,
    pub cookie_secure: Option<bool>,
    pub request_timeout: Duration,
    pub max_retries: u32,
    pub search_max_pages: u32,
    pub search_source_timeout: Duration,
    pub user_agent: String,
    pub fallback_cors_proxy: Option<String>,
    pub api_60s_base: String,
    pub live_allow_private: bool,
    /// 服务端持久化数据目录（SQLite 文件 libretv.db 所在位置）。
    /// 只存「实际有用的数据」：源配置/订阅/设置/播放历史/进度/搜索记录；
    /// 搜索、EPG、画质、测活等 TTL 缓存一律留在内存，不落盘。
    pub data_dir: PathBuf,
    /// 已归一化的预置配置（直接下发给 /api/status）
    pub default_sources: Value,
    pub default_live_sources: Value,
    pub default_subscriptions: Value,
    pub default_recommend_source: Option<String>,
    pub default_image_mode: Option<String>,
    pub version: String,
}

impl Config {
    pub fn from_env() -> Arc<Self> {
        let password = env("PASSWORD", "");
        let session_secret = match env_opt("PROXY_SECRET") {
            Some(s) => s.into_bytes(),
            None => {
                use sha2::{Digest, Sha256};
                // 原版 getSecret(): sha256(PASSWORD + ':libretv::session-salt').digest('hex')
                let digest = Sha256::digest(format!("{password}:libretv::session-salt").as_bytes());
                hex::encode(digest).into_bytes()
            }
        };

        let cookie_secure = match env("COOKIE_SECURE", "").as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };

        Arc::new(Self {
            password,
            session_secret,
            cookie_secure,
            request_timeout: Duration::from_millis(env_num("REQUEST_TIMEOUT", 8000)),
            max_retries: env_num("MAX_RETRIES", 1) as u32,
            search_max_pages: env_num("SEARCH_MAX_PAGES", 5).clamp(1, 50) as u32,
            search_source_timeout: Duration::from_millis(
                env_num("SEARCH_SOURCE_TIMEOUT_MS", 10_000).clamp(3000, 60_000),
            ),
            user_agent: env(
                "USER_AGENT",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36",
            ),
            fallback_cors_proxy: env_opt("FALLBACK_CORS_PROXY"),
            api_60s_base: env("60S_API_BASE", "https://60s.crystelf.top"),
            live_allow_private: env("LIVE_ALLOW_PRIVATE", "") == "1",
            data_dir: PathBuf::from(env("DATA_DIR", "./data")),
            default_sources: parse_env_sources("DEFAULT_SOURCES", "env_"),
            default_live_sources: parse_env_sources("DEFAULT_LIVE_SOURCES", "envlive_"),
            default_subscriptions: parse_env_subscriptions(),
            default_recommend_source: validate_choice(
                "DEFAULT_RECOMMEND_SOURCE",
                &["douban", "bangumi", "hot-list"],
            ),
            default_image_mode: validate_choice("DEFAULT_IMAGE_MODE", &["direct", "proxy"]),
            version: env("APP_VERSION", env!("CARGO_PKG_VERSION")),
        })
    }

    pub fn password_configured(&self) -> bool {
        !self.password.is_empty()
    }
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn env_num(key: &str, default: u64) -> u64 {
    env_opt(key).and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(default)
}

fn validate_choice(key: &str, allowed: &[&str]) -> Option<String> {
    let v = env(key, "").trim().to_lowercase();
    if allowed.contains(&v.as_str()) {
        Some(v)
    } else {
        if !v.is_empty() {
            eprintln!("[libretv-rs] {key} 取值无效，已忽略: {v}");
        }
        None
    }
}

/// DEFAULT_SOURCES / DEFAULT_LIVE_SOURCES：JSON 数组 → 前端 SourceConfig 结构
fn parse_env_sources(key: &str, key_prefix: &str) -> Value {
    let raw = match env_opt(key) {
        Some(v) => v,
        None => return Value::Array(vec![]),
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[libretv-rs] {key} 解析失败，已忽略: {e}");
            return Value::Array(vec![]);
        }
    };
    let arr = match parsed.as_array() {
        Some(a) => a,
        None => return Value::Array(vec![]),
    };
    let mut out = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("").trim();
        let url = item.get("url").and_then(Value::as_str).unwrap_or("").trim().trim_end_matches('/');
        if name.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        let mut obj = serde_json::Map::new();
        obj.insert("key".into(), Value::String(format!("{key_prefix}{i}")));
        obj.insert("name".into(), Value::String(name.to_string()));
        obj.insert("url".into(), Value::String(url.to_string()));
        if let Some(detail) = item.get("detail").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            obj.insert("detail".into(), Value::String(detail.trim().to_string()));
        }
        if let Some(epg) = item.get("epg").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            obj.insert("epg".into(), Value::String(epg.trim().to_string()));
        }
        obj.insert("isAdult".into(), Value::Bool(item.get("isAdult").and_then(Value::as_bool) == Some(true)));
        out.push(Value::Object(obj));
    }
    Value::Array(out)
}

/// DEFAULT_SUBSCRIPTIONS：字符串或 {url,name,refreshDays} 对象
fn parse_env_subscriptions() -> Value {
    let raw = match env_opt("DEFAULT_SUBSCRIPTIONS") {
        Some(v) => v,
        None => return Value::Array(vec![]),
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[libretv-rs] DEFAULT_SUBSCRIPTIONS 解析失败，已忽略: {e}");
            return Value::Array(vec![]);
        }
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    if let Some(arr) = parsed.as_array() {
        for item in arr {
            let url = match item {
                Value::String(s) => s.trim().trim_end_matches('/').to_string(),
                Value::Object(_) => item
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches('/')
                    .to_string(),
                _ => String::new(),
            };
            if !(url.starts_with("http://") || url.starts_with("https://")) || !seen.insert(url.clone()) {
                continue;
            }
            let mut obj = serde_json::Map::new();
            obj.insert("url".into(), Value::String(url));
            if let Some(name) = item.get("name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                obj.insert("name".into(), Value::String(name.trim().to_string()));
            }
            if let Some(days) = item.get("refreshDays").and_then(|v| {
                v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            }) {
                if days > 0 {
                    obj.insert("refreshDays".into(), Value::from(days.clamp(1, 365)));
                }
            }
            out.push(Value::Object(obj));
        }
    }
    Value::Array(out)
}
