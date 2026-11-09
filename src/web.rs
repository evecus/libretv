//! 静态资源托管：rust-embed 内嵌前端产物，配合 SPA 路由回退。
//! 产物来自 `output: 'export'`：index.html / watch.html / live.html / about.html / 404.html。

use axum::{
    body::Body,
    http::{header, Uri},
    response::Response,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct Assets;

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/').to_string();
    let path = if path.is_empty() { "index.html".to_string() } else { path };

    if let Some(file) = Assets::get(&path) {
        return file_response(&path, file.data.to_vec(), 200);
    }

    // 前端 useRouter 产生 /watch?... 这类无扩展名路径，映射到导出的 HTML
    let page = match path.as_str() {
        "watch" | "watch/" => Some("watch.html"),
        "live" | "live/" => Some("live.html"),
        "about" | "about/" => Some("about.html"),
        "" | "/" => Some("index.html"),
        _ => None,
    };
    if let Some(p) = page {
        if let Some(file) = Assets::get(p) {
            return file_response(p, file.data.to_vec(), 200);
        }
    }
    // 未知路径：交给前端 404 页（Next 静态导出产物）
    match Assets::get("404.html") {
        Some(file) => file_response("404.html", file.data.to_vec(), 404),
        None => Response::builder().status(404).body(Body::empty()).unwrap(),
    }
}

fn file_response(path: &str, data: Vec<u8>, status: u16) -> Response {
    let content_type = if path.ends_with(".webmanifest") {
        "application/manifest+json".to_string()
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8".to_string()
    } else {
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string()
    };
    // _next 下的文件带内容 hash，可长缓存；HTML 不缓存以免版本错配
    let cache = if path.starts_with("_next/") || path.starts_with("icons/") {
        "public, max-age=31536000, immutable"
    } else if path.ends_with(".html") {
        "no-cache"
    } else {
        "public, max-age=3600"
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, cache)
        .body(Body::from(data))
        .unwrap()
}
