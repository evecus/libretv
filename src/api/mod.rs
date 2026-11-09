//! 路由表：16 个 /api 端点（与前端 client-api.ts 契约一致）+ 静态资源兜底。

use axum::{
    routing::{get, post, put},
    Router,
};

use crate::state::AppState;

pub mod basic;
pub mod data;
pub mod live;
pub mod proxy;
pub mod recommend;
pub mod search;
pub mod source;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/status", get(basic::status))
        .route(
            "/api/auth",
            post(basic::login).get(basic::auth_status).delete(basic::logout),
        )
        .route("/api/search", post(search::search))
        .route("/api/detail", get(search::detail))
        .route("/api/source/test", post(search::source_test))
        .route("/api/proxy", get(proxy::proxy))
        .route("/api/proxy/{*url}", get(proxy::proxy_path))
        .route("/api/live/stream", get(proxy::live_stream))
        .route("/api/live/stream/{*url}", get(proxy::live_stream_path))
        .route("/api/live/playlist", get(live::playlist))
        .route("/api/live/epg", get(live::epg))
        .route("/api/live/probe", post(live::probe))
        .route("/api/douban", get(recommend::douban))
        .route("/api/bangumi/calendar", get(recommend::bangumi))
        .route("/api/hot-list", get(recommend::hot_list))
        .route("/api/source-list", get(source::source_list))
        .route("/api/publish", post(source::publish))
        .route("/api/quality", post(source::quality))
        // 服务端持久化数据（配置/源/订阅/历史/进度/设置），缓存不走这里
        .route("/api/data", get(data::all))
        .route(
            "/api/data/{section}",
            put(data::upsert).delete(data::remove),
        )
        .fallback(web::serve)
        .with_state(state)
}

use crate::web;
