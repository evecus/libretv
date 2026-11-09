#!/usr/bin/env bash
# 构建前端静态产物（唯一需要 Node.js 的环节，产物会被 rust-embed 内嵌进二进制）。
# Next.js `output: 'export'` 与 Route Handlers 不能共存，故构建期把 src/app/api 临时移出，
# 构建结束（无论成功失败）都恢复，保证前端源码保持不变。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WEB="$ROOT/web"
STASH="$ROOT/.web-api-stash"
DIST="$ROOT/web/dist"

cleanup() {
  if [[ -d "$STASH" ]]; then
    rm -rf "$WEB/src/app/api"
    mv "$STASH" "$WEB/src/app/api"
  fi
}
trap cleanup EXIT

cd "$WEB"
rm -rf "$STASH" out "$DIST"
mv src/app/api "$STASH"

echo "==> npm ci"
npm ci --no-audit --no-fund

echo "==> next build (output: export)"
NEXT_TELEMETRY_DISABLED=1 npm run build

cp -a out "$DIST"
echo "==> 静态产物已生成: $DIST"
find "$DIST" -type f | wc -l
