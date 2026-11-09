#!/usr/bin/env bash
# 本地交叉编译多架构二进制（依赖 cargo-zigbuild + zig；rustls 无需 OpenSSL 交叉工具链）。
# 前端产物已随仓库提供（web/dist），因此这里完全不需要 Node.js。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGETS=(
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-musl
  armv7-unknown-linux-musleabihf
  i686-unknown-linux-musl
)

if ! command -v cargo-zigbuild >/dev/null 2>&1; then
  echo "缺少 cargo-zigbuild，请先安装："
  echo "  pip3 install ziglang && cargo install cargo-zigbuild --locked"
  exit 1
fi

mkdir -p dist
for t in "${TARGETS[@]}"; do
  echo "==> 编译 $t"
  rustup target add "$t" 2>/dev/null || true
  cargo zigbuild --release --locked --target "$t"
  cp "target/$t/release/libretv-rs" "dist/libretv-rs-$t"
  echo "    -> dist/libretv-rs-$t"
done
(cd dist && sha256sum libretv-rs-* > SHA256SUMS)
echo "完成：$(ls dist | wc -l) 个文件"
