#!/usr/bin/env bash
# 用当前工作树重建镜像并重启容器。部署目标就是这台机器 —— 不再往任何云推。
#
#   ./scripts/deploy.sh                 # 闸门（npm test）→ 构建 → up -d → 探活
#   ./scripts/deploy.sh --skip-checks   # 跳过本地闸门（紧急回滚时用，别当常态）
#   ./scripts/deploy.sh --logs          # 结束后跟踪日志
#
# 为什么把 npm test 放在构建前面：42 项断言里有一批直接跑在真实 MySQL 上，测的正是
# 迁移最容易悄悄变味的那几处 —— 抢闸的 changes===1 判定（FOUND_ROWS）、CAS 提交、
# 200 条整池过 LONGTEXT、uuid 按字节比较。镜像构建本身不跑测试；少这道闸门，
# 一个方言差异就是要到某台机器真的去续签那一刻才暴露，而那时的代价是一池凭证。
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

SKIP_CHECKS=0
SHOW_LOGS=0
for arg in "$@"; do
  case "$arg" in
    --skip-checks) SKIP_CHECKS=1 ;;
    --logs) SHOW_LOGS=1 ;;
    *) echo "未知参数：$arg" >&2; exit 2 ;;
  esac
done

[ -f .env ] || { echo "❌ 缺少 .env，先执行 ./scripts/db-init.sh" >&2; exit 1; }
docker network inspect mysql-server_default >/dev/null 2>&1 || {
  echo "❌ 网络 mysql-server_default 不存在，先启动数据库：../../mysql-server/scripts/start.sh" >&2
  exit 1
}
# compose 以 external 引用 gw_default，缺它 `docker compose up` 直接失败 —— 提前说清原因
docker network inspect gw_default >/dev/null 2>&1 || {
  echo "❌ 网络 gw_default 不存在，先启动共享公网入口：../../gw（./scripts/gw-join.sh 可一并接好）" >&2
  exit 1
}

if [ "$SKIP_CHECKS" -eq 0 ]; then
  echo "==> 测试（真实 MySQL：闸的原子性 + CAS + 规范化 + 方言语义）"
  [ -f .env.test ] || echo "   ⚠️  没有 .env.test，测试会退回读 .env 并被「必须是 _test 库」的闸门拦下"
  npm test
fi

echo "==> 构建镜像"
docker compose build

echo "==> 启动容器"
docker compose up -d

echo "==> 等待健康检查"
for _ in $(seq 1 30); do
  status=$(docker inspect -f '{{.State.Health.Status}}' cred-broker 2>/dev/null || echo starting)
  [ "$status" = healthy ] && break
  sleep 2
done
docker compose ps

bind=$(sed -n 's/^APP_BIND_ADDR=//p' .env | head -1)
port=$(sed -n 's/^APP_PORT=//p' .env | head -1)
bind=${bind:-127.0.0.1}; port=${port:-7003}
echo ""
echo "✅ 部署完成： http://${bind}:${port}   （健康检查：${status}）"

# 两个探活各有其人：/healthz 真查库（compose 用的那个），/v1/health 只回闸参数（运维看部署用的那个）
if health=$(curl -fsS --max-time 5 "http://${bind}:${port}/healthz"); then
  echo "   /healthz    ${health}"
fi
if h=$(curl -fsS --max-time 5 "http://${bind}:${port}/v1/health"); then
  echo "   /v1/health  ${h}"
  lease=$(printf '%s' "$h" | sed -n 's/.*"lease_ms":\([0-9]*\).*/\1/p')
  [ "${lease:-180000}" = "180000" ] || echo "   ℹ️  lease_ms 已不是默认的 180000：它必须大于「整池签完 + 提交」的最坏耗时，调小会开出租约被别机接管的窗口"
fi

# 公网入口由共享的 ../../gw 提供：网关上有本域名的 vhost 才算接入
GW_DIR="${GW_DIR:-$ROOT_DIR/../../gw}"
GW_CONF="$GW_DIR/conf.d/cred-broker.conf"
if [ -f "$GW_CONF" ]; then
  gw_state=$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' gw 2>/dev/null || echo 未启动)
  echo "   公网入口： https://cred-broker.sloan.dpdns.org   （网关 gw：${gw_state}）"
  # 上游必须是容器网络里的 80。写成宿主端口 7003 时网关连不上（那端口只在回环上），
  # 而本机 curl 因为 hosts 接管一切正常 —— 只有从网关那一侧才看得出来。
  if grep -qE 'cred-broker:(7003|8789)' "$GW_CONF"; then
    cat <<'EOF'
   ⚠️  网关的上游端口是宿主发布端口，gw 容器连不到，域名会全 502。
       修：./scripts/gw-join.sh --overwrite
       验：docker exec gw wget -qO- http://cred-broker:80/healthz
EOF
  fi
  if ! docker exec gw wget -qO- -T 5 "http://cred-broker:80/healthz" >/dev/null 2>&1; then
    echo "   ⚠️  gw 容器打不到 cred-broker:80（公网域名会 502）：检查容器是否在跑、compose 的 gw_default 网络是否接上"
  fi
else
  cat <<'EOF'
   ⚠️  公网域名还没接到本机（../../gw 上没有 cred-broker.conf）。
       云上那份 Worker 与 D1 已删除，域名现在没有别的应答方 —— 绑了池的其他机器会连不上
       broker（它们会退回只用本地凭证，不会双写，但也拿不到跨机互斥闸）。
       接上：./scripts/gw-join.sh
EOF
fi

[ "$SHOW_LOGS" -eq 1 ] && exec docker compose logs -f
