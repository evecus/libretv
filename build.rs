fn main() {
    // rust-embed 在编译期读取 web/dist。缺失时生成占位页，保证 cargo build 不会直接失败，
    // 但会明确告警：需要先运行 scripts/build-web.sh（唯一需要 Node.js 的环节）。
    let dist = std::path::Path::new("web/dist");
    if !dist.join("index.html").exists() {
        let _ = std::fs::create_dir_all(dist);
        let _ = std::fs::write(
            dist.join("index.html"),
            b"<!doctype html><html><body><h1>LibreTV</h1><p>frontend assets missing, run scripts/build-web.sh</p></body></html>",
        );
        println!("cargo:warning=web/dist 缺失，已生成占位页；请运行 scripts/build-web.sh 后重新构建");
    }
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-env-changed=LIBRETV_BUILD_FRONTEND");
}
