#!/usr/bin/env bash
# 把本项目接到共享公网入口 ../../gw（可重复执行）。
#
#   ./scripts/gw-join.sh [公网域名]        # 默认 cred-broker.sloan.dpdns.org
#
# 依次做三件事：
#   1. 确认 ../../gw 存在且网关容器在跑
#   2. 确保共享网络 gw_default 存在（本项目 compose 以 external 引用它）
#   3. 调 ../../gw/scripts/gw-add-host.sh：生成 nginx vhost + 网关内 reload
#
# 域名一直是 cred-broker.sloan.dpdns.org（Cloudflare Worker 时代用的同一个），这是刻意的：
# qoder-assistant / traework-assistant / workbuddy-assistant 三个应用都把它写成
# **编译期常量**（src-tauri/src/broker.rs 的 BASE）。换名等于改三处代码 + 重新构建 +
# 让用户重装 —— 而服务端换引擎本来是可以对客户端完全无感的。
# DNS 侧零操作：隧道带的是 *.sloan.dpdns.org 通配记录，网关只是多了一个按 Host 转发的 server 块。
#
# 前置条件只剩一条：**容器里得真有数据**（库里没有对应 uuid 的池 = 所有客户端拿到 404 gone，
# 然后各自重新建池，于是同一批账号出现两份互不知情的池 —— 这正是这把闸要防的事）。
# 当年排在后面的「去 Cloudflare 删 custom domain 路由」已经做完：Worker 与 D1 都删了，
# 这个名字现在只有本机这一条应答路径。
#
# ⚠️ 上游端口必须是容器名 + 容器端口（cred-broker:8787），不能写宿主发布端口（8789）：
#    那个端口只绑在宿主回环上，gw 容器连不到，结果是**经域名进来的流量全 502**，
#    而本机 curl 因为 /etc/hosts 接管照样正常。本脚本会检测并重写（--overwrite）。
#    真正的判据是从网关那一侧探：docker exec gw wget -qO- http://cred-broker:8787/healthz
#
# 这里**没有** TRUST_PROXY 那一步（notify-hub 有）：本服务从不回绝对 URL，
# src/cred.js 只用 new URL(request.url).pathname 做路由，容器看到 http 还是 https
# 对结果没有任何影响。加一个没人读的开关只是多一处要记的约定。
#
# 撤销公网访问：删掉 ../../gw/conf.d/cred-broker.conf 并在网关内 reload
# （docker exec gw nginx -s reload），另可选清掉本机 hosts 记录，见 gw-local-takeover.sh。
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

PUBLIC_HOSTNAME="${1:-cred-broker.sloan.dpdns.org}"
CONTAINER_TARGET="cred-broker:8787"   # 必须是容器名：网关容器里的 127.0.0.1 是它自己
GW_DIR="${GW_DIR:-$ROOT_DIR/../../gw}"
NETWORK=gw_default

[ -f "$GW_DIR/scripts/gw-add-host.sh" ] || {
  echo "❌ 找不到共享入口项目 $GW_DIR，先建好并执行 ../../gw/scripts/gw-init.sh" >&2
  exit 1
}
[ "$(docker inspect -f '{{.State.Running}}' gw 2>/dev/null || echo false)" = "true" ] || {
  echo "❌ 网关 gw 没在跑：cd $GW_DIR && docker compose up -d" >&2
  exit 1
}
[ "$(docker inspect -f '{{.State.Running}}' cred-broker 2>/dev/null || echo false)" = "true" ] || {
  echo "❌ 容器 cred-broker 没在跑，先 ./scripts/deploy.sh" >&2
  exit 1
}

echo "==> 确保共享网络 $NETWORK 存在"
# 正常由 ../../gw/scripts/gw-init.sh 创建；这里兜底一次，让 clone 后单独跑本脚本也能成。
docker network create "$NETWORK" >/dev/null 2>&1 && echo "    已创建" || echo "    已存在，跳过"

echo "==> 让容器挂上 $NETWORK 并重启到位"
# 不加 --force-recreate：compose 按配置自己判断要不要重建，没变就不该白重启一次。
docker compose up -d

echo "==> 登记域名 $PUBLIC_HOSTNAME → $CONTAINER_TARGET"
# vhost 已存在时带 --overwrite 重写：这台机器上出现过管理台手写的 conf 把上游写成
# 宿主机发布端口（cred-broker:8789）的情况 —— 那个端口只在宿主回环上存在，容器网内
# 连不上，网关一律 502。本脚本以「容器名:8787」为唯一正确写法，重跑一次即可纠正。
gw_args=("$PUBLIC_HOSTNAME" "$CONTAINER_TARGET")
if [ -f "$GW_DIR/conf.d/${PUBLIC_HOSTNAME%%.*}.conf" ]; then
  echo "    ${PUBLIC_HOSTNAME%%.*}.conf 已存在 → 带 --overwrite 重写"
  gw_args+=(--overwrite)
fi
( cd "$GW_DIR" && ./scripts/gw-add-host.sh "${gw_args[@]}" )

echo ""
echo "✅ 就绪。https://$PUBLIC_HOSTNAME/v1/health"
echo "   上游真值（网关那一侧，这条才算数）："
echo "     docker exec gw wget -qO- http://$CONTAINER_TARGET/healthz"
echo "   公网验证（--resolve 钉到 Cloudflare 边缘，绕开本机 hosts 的接管）："
echo "     curl -s https://$PUBLIC_HOSTNAME/healthz --resolve $PUBLIC_HOSTNAME:443:104.16.0.1"
echo "   本机验证： curl -4 -sk https://$PUBLIC_HOSTNAME/v1/health"
echo "   网关健康： docker inspect -f '{{.State.Health.Status}}' gw    日志：docker logs gw"
echo ""
echo "接完之后要做的一次核对：三个应用各自跑一次续签，确认池版本号 +1、没有第二个 uuid 冒出来。"
