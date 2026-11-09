//! 上游请求层：SSRF 防护 + 手动逐跳重定向 + 只等响应头的超时。
//!
//! 安全要点（对齐原版 ssrf.ts / fetch-utils.ts）：
//! 1. 协议白名单 + 私网/保留 IP 字面量拦截；
//! 2. DNS 解析后校验**全部** A/AAAA 结果，任一私网即拒绝（防 DNS rebinding）；
//! 3. 把校验通过的 IP 用 `ClientBuilder::resolve` 固定为实际连接目标，杜绝 TOCTOU；
//! 4. 关闭自动重定向，每跳重新校验（否则公网 302 可跳进内网）；
//! 5. 每请求独立 Client、禁用连接保活，不留跨请求复用窗口；
//! 6. 超时只作用于「等响应头」，body 不做总限时，否则 HTTP-FLV 长连接会被切断。

use std::{net::IpAddr, sync::Arc, time::Duration};

use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use reqwest::{redirect::Policy, Client, RequestBuilder};
use serde_json::Value;
use url::Url;

use crate::{config::Config, error::ApiError};

const BLOCKED_HOSTNAMES: &[&str] = &["localhost", "127.0.0.1", "0.0.0.0", "::1"];
const REDIRECT_CODES: &[u16] = &[301, 302, 303, 307, 308];
const MAX_HOPS: usize = 5;

pub struct Upstream {
    cfg: Arc<Config>,
}

#[derive(Clone)]
pub struct FetchOptions {
    pub headers: HeaderMap,
    /// 仅等待响应头的超时
    pub timeout: Duration,
    pub allow_private: bool,
    pub method: Method,
    pub body: Option<Vec<u8>>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        Self {
            headers,
            timeout: Duration::from_millis(8000),
            allow_private: false,
            method: Method::GET,
            body: None,
        }
    }
}

impl FetchOptions {
    pub fn with_timeout(mut self, ms: u64) -> Self {
        self.timeout = Duration::from_millis(ms);
        self
    }
    pub fn allow_private(mut self, allow: bool) -> Self {
        self.allow_private = allow;
        self
    }
    pub fn header(mut self, name: &'static str, value: &str) -> Self {
        if let Ok(v) = HeaderValue::from_str(value) {
            self.headers.insert(name, v);
        }
        self
    }
    pub fn json_accept(mut self) -> Self {
        self.headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        self
    }
}

pub struct FetchResult {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub final_url: Url,
    /// 未读取的响应流（调用方决定整块读取还是流式透传）
    pub response: reqwest::Response,
}

impl Upstream {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }

    /// 只做校验不发请求（search / detail / source-list 等用户可控地址入口）
    pub async fn check_url(&self, url: &str, allow_private: bool) -> Result<(), ApiError> {
        let parsed = Url::parse(url).map_err(|_| ApiError::BadRequest("无效的 URL".into()))?;
        verify_target(&parsed, allow_private).await?;
        Ok(())
    }

    /// 单次请求（含手动逐跳重定向）。返回的 response 尚未读取 body。
    pub async fn fetch(&self, url: &str, opts: &FetchOptions) -> Result<FetchResult, ApiError> {
        let mut current = Url::parse(url).map_err(|_| ApiError::Forbidden("无效的 URL".into()))?;
        let mut method = opts.method.clone();
        let mut body = opts.body.clone();

        for _ in 0..=MAX_HOPS {
            let (resolve_host, resolve_addr) = verify_target(&current, opts.allow_private).await?;
            let client = build_client(&self.cfg, resolve_host.as_deref(), resolve_addr)?;

            let mut req: RequestBuilder = client
                .request(method.clone(), current.clone())
                .headers(opts.headers.clone());
            if let Some(b) = body.take() {
                req = req.body(b);
            }

            let response = match tokio::time::timeout(opts.timeout, req.send()).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    // 网络错误不重试整链，交由外层 fetch_with_retries 决定
                    return Err(ApiError::BadGateway(format!("上游请求失败: {e}")));
                }
                Err(_) => return Err(ApiError::BadGateway("上游响应头等待超时".into())),
            };

            let status = response.status();
            if REDIRECT_CODES.contains(&status.as_u16()) {
                if let Some(loc) = response.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()) {
                    if let Ok(next) = current.join(loc) {
                        // 丢弃重定向响应体，进入下一跳（下一跳会重新做完整 SSRF 校验）
                        drop(response);
                        current = next;
                        method = Method::GET;
                        body = None;
                        continue;
                    }
                }
            }

            let headers = clone_headers(response.headers());
            let final_url = current.clone();
            return Ok(FetchResult {
                status: StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                headers,
                final_url,
                response,
            });
        }
        Err(ApiError::BadGateway("重定向次数过多".into()))
    }

    pub async fn fetch_with_retries(
        &self,
        url: &str,
        opts: &FetchOptions,
        retries: u32,
    ) -> Result<FetchResult, ApiError> {
        let mut last = ApiError::BadGateway("代理请求失败".into());
        for attempt in 0..=retries {
            match self.fetch(url, opts).await {
                Ok(r) => return Ok(r),
                Err(e) => last = e,
            }
            if attempt < retries {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
        Err(last)
    }

    /// 整块读取 JSON（总时长受 timeout 约束）
    pub async fn json(&self, url: &str, opts: &FetchOptions) -> Result<Value, ApiError> {
        let timeout = opts.timeout;
        let bytes = self.read_bytes_limited(url, opts, timeout).await?;
        let text = decode_body(&bytes);
        serde_json::from_str(&text).map_err(|e| ApiError::BadGateway(format!("JSON 解析失败: {e}")))
    }

    pub async fn text(&self, url: &str, opts: &FetchOptions) -> Result<String, ApiError> {
        let timeout = opts.timeout;
        let bytes = self.read_bytes_limited(url, opts, timeout).await?;
        Ok(decode_body(&bytes))
    }

    pub async fn bytes(&self, url: &str, opts: &FetchOptions) -> Result<Vec<u8>, ApiError> {
        let timeout = opts.timeout;
        self.read_bytes_limited(url, opts, timeout).await
    }

    async fn read_bytes_limited(
        &self,
        url: &str,
        opts: &FetchOptions,
        timeout: Duration,
    ) -> Result<Vec<u8>, ApiError> {
        let fut = async {
            let r = self.fetch(url, opts).await?;
            let b = r
                .response
                .bytes()
                .await
                .map_err(|e| ApiError::BadGateway(format!("读取上游响应失败: {e}")))?;
            Ok::<_, ApiError>(b.to_vec())
        };
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| ApiError::BadGateway("上游请求超时".into()))?
    }
}

/// 解析 + 校验目标，返回需要固定到连接的 (hostname, SocketAddr)
async fn verify_target(url: &Url, allow_private: bool) -> Result<(Option<String>, Option<std::net::SocketAddr>), ApiError> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ApiError::Forbidden("目标地址不在允许范围内（仅支持公网 http/https）".into()));
    }
    let raw_host = url.host_str().ok_or_else(|| ApiError::Forbidden("目标地址缺少主机名".into()))?;
    let host = raw_host.trim_matches(|c| c == '[' || c == ']');
    let port = url.port_or_known_default().unwrap_or(80);

    if !allow_private && BLOCKED_HOSTNAMES.contains(&host.to_lowercase().as_str()) {
        return Err(ApiError::Forbidden("目标主机名在黑名单中".into()));
    }

    // IP 字面量：直接判定，无需 resolve
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !allow_private && is_private_ip(ip) {
            return Err(ApiError::Forbidden("不允许访问私有/保留网络地址".into()));
        }
        return Ok((None, None));
    }

    // 域名：解析全部地址，任一私网即拒绝；然后固定选中的公网 IP 作为连接目标
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| ApiError::BadGateway(format!("DNS 解析失败: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(ApiError::BadGateway(format!("未找到主机地址: {host}")));
    }
    for a in &addrs {
        if !allow_private && is_private_ip(a.ip()) {
            return Err(ApiError::Forbidden("目标地址解析到私有/保留网络".into()));
        }
    }
    // 双栈环境下优先 IPv4：部分网络 IPv6 出网不通，Node 的 dns.lookup 默认也是 IPv4 优先
    let chosen = addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied();
    Ok((Some(host.to_string()), chosen))
}

fn build_client(
    cfg: &Config,
    resolve_host: Option<&str>,
    resolve_addr: Option<std::net::SocketAddr>,
) -> Result<Client, ApiError> {
    let mut b = Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .user_agent(cfg.user_agent.clone())
        .gzip(true)
        .brotli(true)
        .pool_max_idle_per_host(0)
        .http1_only();
    if let (Some(h), Some(a)) = (resolve_host, resolve_addr) {
        b = b.resolve(h, a);
    }
    b.build().map_err(|e| ApiError::Internal(format!("构建 HTTP 客户端失败: {e}")))
}

fn clone_headers(src: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in src.iter() {
        if let Ok(name) = axum::http::header::HeaderName::from_bytes(k.as_str().as_bytes()) {
            if let Ok(val) = HeaderValue::from_bytes(v.as_bytes()) {
                out.insert(name, val);
            }
        }
    }
    out
}

/// 采集站常见 GBK 响应；UTF-8 失败时按 GBK 解码
pub fn decode_body(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.trim_start_matches('\u{feff}').to_string(),
        Err(_) => {
            let (cow, _, _) = encoding_rs::GBK.decode(bytes);
            cow.to_string()
        }
    }
}

/// 私网 / 回环 / 链路本地 / 保留地址判定（含 IPv4-mapped IPv6 归一化）
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 0
                || o[0] == 10
                || o[0] == 127
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 169 && o[1] == 254)
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || o[0] >= 224
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            // ::ffff:a.b.c.d / ::a.b.c.d
            if s[0..5] == [0, 0, 0, 0, 0] && (s[5] == 0 || s[5] == 0xffff) {
                let o = [(s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8];
                return is_private_ip(IpAddr::V4(std::net::Ipv4Addr::new(o[0], o[1], o[2], o[3])));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10
        }
    }
}
