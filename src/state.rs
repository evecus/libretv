//! 全局状态：配置、上游客户端、TTL 缓存、登录限流、按 host 并发闸门。

use std::{sync::Arc, time::Duration};

use dashmap::DashMap;
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{config::Config, net::Upstream, store::Store, util::now_ms};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub upstream: Arc<Upstream>,
    pub cache: Arc<TtlCache>,
    pub limiter: Arc<RateLimiter>,
    pub host_gate: Arc<HostGate>,
    /// 服务端持久化（SQLite）；缓存一律走上面的内存 TtlCache，不落盘
    pub store: Arc<Store>,
}

impl AppState {
    pub fn new(cfg: Arc<Config>) -> Self {
        let cache = Arc::new(TtlCache::new());
        let c = cache.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                c.cleanup();
            }
        });
        let store = Store::open(&cfg.data_dir).unwrap_or_else(|e| {
            eprintln!("[libretv-rs] 数据目录初始化失败: {e}");
            std::process::exit(1);
        });
        Self {
            upstream: Arc::new(Upstream::new(cfg.clone())),
            cfg,
            cache,
            limiter: Arc::new(RateLimiter::new()),
            host_gate: Arc::new(HostGate::new(3)),
            store,
        }
    }
}

/// 简易 TTL 缓存：对齐原版 getCache/setCache（搜索 60s、详情 60s、画质 30min 等）
pub struct TtlCache {
    map: DashMap<String, (i64, Value)>,
}

impl TtlCache {
    pub fn new() -> Self {
        Self { map: DashMap::new() }
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        let entry = self.map.get(key)?;
        if entry.0 > now_ms() {
            Some(entry.1.clone())
        } else {
            drop(entry);
            self.map.remove(key);
            None
        }
    }

    pub fn set(&self, key: impl Into<String>, value: Value, ttl: Duration) {
        self.map.insert(
            key.into(),
            (now_ms() + ttl.as_millis() as i64, value),
        );
    }

    pub fn cleanup(&self) {
        let now = now_ms();
        self.map.retain(|_, (expire, _)| *expire > now);
    }
}

/// 登录限流：10 次 / 10 分钟（原版 auth.ts 的 attemptMap）
pub struct RateLimiter {
    map: DashMap<String, (u32, i64)>,
    max_attempts: u32,
    window_ms: i64,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self { map: DashMap::new(), max_attempts: 10, window_ms: 10 * 60 * 1000 }
    }

    pub fn check(&self, ip: &str) -> bool {
        let now = now_ms();
        let mut entry = self.map.entry(ip.to_string()).or_insert((0, now + self.window_ms));
        if now > entry.1 {
            *entry = (1, now + self.window_ms);
            return true;
        }
        if entry.0 >= self.max_attempts {
            return false;
        }
        entry.0 += 1;
        true
    }

    pub fn clear(&self, ip: &str) {
        self.map.remove(ip);
    }
}

/// 按 host 限并发（原版 host-gate.ts）：同一 host 最多 N 个在途请求
pub struct HostGate {
    per_host: usize,
    gates: DashMap<String, Arc<Semaphore>>,
}

impl HostGate {
    pub fn new(per_host: usize) -> Self {
        Self { per_host, gates: DashMap::new() }
    }

    pub async fn acquire(&self, url: &str) -> OwnedSemaphorePermit {
        let host = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_else(|| url.to_string());
        let sem = self
            .gates
            .entry(host)
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_host)))
            .clone();
        sem.acquire_owned().await.expect("semaphore closed")
    }
}

use url::Url;
