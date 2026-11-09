//! LibreTV (Rust 后端)：单二进制、前端静态内嵌、运行时零 Node.js 依赖。
//!
//! 启动示例：
//!   PASSWORD=change-me HOST=0.0.0.0 PORT=8080 ./libretv-rs
//!
//! 全部环境变量契约见 config.rs；前端产物由 scripts/build-web.sh 生成后内嵌。

mod api;
mod auth;
mod cms;
mod config;
mod error;
mod m3u8;
mod net;
mod state;
mod store;
mod util;
mod web;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "libretv-rs", about = "LibreTV 的 Rust 单二进制实现")]
struct Cli {
    #[arg(long, env = "HOST", default_value = "0.0.0.0")]
    host: String,
    #[arg(long, env = "PORT", default_value_t = 8080)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = config::Config::from_env();
    if !cfg.password_configured() {
        tracing::warn!("未设置 PASSWORD，站点会提示管理员配置后才可用（/api/status 返回 503 引导）");
    }
    tracing::info!(
        "LibreTV {} 启动，前端已内嵌，运行时无 Node.js",
        cfg.version
    );
    tracing::info!(
        "服务端数据目录 {}（SQLite，缓存仅存内存不落盘）",
        store::Store::db_path(&cfg.data_dir).display()
    );

    let state = state::AppState::new(cfg);
    let app = api::router(state);
    let addr = format!("{}:{}", cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("监听 http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
