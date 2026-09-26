#!/usr/bin/env bash
# 一次性初始化（可重复执行）：
#   1. 在共享 mysql-server 容器里建本项目专用库 + 专用账号（不碰该服务上的其它库）
#   2. 应用 db/schema.mysql.sql（本机库结构的唯一真源）
#   3. 顺带把测试库 cred_broker_test 也建好，并生成 .env.test
#      —— 测试每次运行都 DROP 本库全部表再按 schema 重建，所以必须是独立的库，见 test/mysql-env.js
#   4. 生成 .env（随机库密码）
#
# 前置：数据库容器已在跑
#   ../../mysql-server/scripts/start.sh
#
# 重复执行是安全的：库/用户用 IF NOT EXISTS 与 ALTER USER（幂等重置密码），
# 建表语句全是 CREATE TABLE IF NOT EXISTS；已存在的 .env 不会被覆盖。
#
# 与 notify-hub 那份的差别只有一处，但是本质差别：这里**没有 JWT_SECRET**。
# 本服务的凭据就是池 uuid 本身（README「它不做什么」），所以迁移不存在
# 「换了签名密钥导致所有端掉线」这一步 —— 客户端侧零改动，只需数据搬过来。
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

DB_NAME="${DB_NAME:-cred_broker}"
DB_USER="${DB_USER:-cred_broker}"
TEST_DB_NAME="${TEST_DB_NAME:-cred_broker_test}"
MYSQL_CONTAINER="${MYSQL_CONTAINER:-mysql-server}"
# 本项目按约定留在 ai-assistant 仓库内，所以共享服务在上一层的同级目录
MYSQL_PROJECT_DIR="${MYSQL_PROJECT_DIR:-$(dirname "$(dirname "$ROOT_DIR")")/mysql-server}"

password() { openssl rand -base64 48 | tr -dc 'A-Za-z0-9' | head -c "$1"; }

[ -f "$MYSQL_PROJECT_DIR/.env" ] || {
  echo "❌ 找不到 ${MYSQL_PROJECT_DIR}/.env（mysql-server 项目的密码文件）" >&2
  exit 1
}
docker inspect -f '{{.State.Running}}' "$MYSQL_CONTAINER" 2>/dev/null | grep -q true || {
  echo "❌ 容器 ${MYSQL_CONTAINER} 未运行，先执行：${MYSQL_PROJECT_DIR}/scripts/start.sh" >&2
  exit 1
}
[ -f db/schema.mysql.sql ] || { echo "❌ 缺少 db/schema.mysql.sql" >&2; exit 1; }

# root 密码只从 mysql-server 项目读，绝不写进本项目的任何文件。
# MYSQL_ROOT_PASSWORD=... 可临时覆盖 —— 用途是该项目的 .env 与实际数据卷不一致时
# （这台机器上发生过：容器 healthcheck 的 mysqladmin ping 即使 1045 也输出 "mysqld is alive"
#  并退出 0，于是「healthy」掩盖了 root 根本连不上），一次跑通而不必改别人的文件。
ROOT_PW="${MYSQL_ROOT_PASSWORD:-$(sed -n 's/^MYSQL_ROOT_PASSWORD=//p' "$MYSQL_PROJECT_DIR/.env" | head -1)}"
[ -n "$ROOT_PW" ] || { echo "❌ ${MYSQL_PROJECT_DIR}/.env 里没有 MYSQL_ROOT_PASSWORD" >&2; exit 1; }

mysql_admin() {
  docker exec -i "$MYSQL_CONTAINER" mysql -uroot -p"$ROOT_PW" --default-character-set=utf8mb4 "$@"
}

# 先探一次再动手：否则会在建库语句上撞出一句看不懂的 ERROR 1045，
# 而那时 .env 已经生成过了，现场更容易误判成「脚本把库写坏了」。
mysql_admin -N -e 'select 1' >/dev/null 2>&1 || {
  echo "❌ root 连不上 ${MYSQL_CONTAINER}（ERROR 1045）。两处候选：
     a) ${MYSQL_PROJECT_DIR}/.env 的 MYSQL_ROOT_PASSWORD 与数据卷不一致 → 修那个文件，或
     b) 本次临时覆盖：MYSQL_ROOT_PASSWORD='<真实密码>' ./scripts/db-init.sh
   注意容器 healthcheck 用的是 mysqladmin ping，它即使鉴权失败也会报 healthy，不可作为依据。" >&2
  exit 1
}

echo "🔑 生成/沿用 ${ROOT_DIR}/.env"
if [ ! -f .env ]; then
  cat > .env <<EOF
DB_HOST=mysql
DB_PORT=3306
DB_NAME=${DB_NAME}
DB_USER=${DB_USER}
DB_PASSWORD=$(password 24)
LEASE_MS=180000
FAIL_COOLDOWN_MS=300000
APP_BIND_ADDR=127.0.0.1
APP_PORT=8789
EOF
  chmod 600 .env
else
  echo "ℹ️  .env 已存在，沿用其中凭证；只补缺失项"
fi
grep -q "^DB_PASSWORD=.\+" .env || { printf 'DB_PASSWORD=%s\n' "$(password 24)" >>.env; echo "🔑 已补齐 DB_PASSWORD"; }

DB_PASSWORD="$(sed -n 's/^DB_PASSWORD=//p' .env | head -1)"

echo "🗄️  建库 ${DB_NAME} / ${TEST_DB_NAME} 与专用账号 ${DB_USER}"
# 字符集/排序规则在库级就定成 utf8mb4_bin：表级语句里也写了，两处一致才不会出现
# 「手工建的临时表用了 ci 排序，uuid / key 的大小写敏感性跟生产不一样」。
# 账号只从 compose 网络连这一个库，所以 host 用 '%' 是这里的实际最小授权面。
TEST_DB_PASSWORD="$(password 24)"
if [ -f .env.test ]; then
  # 已有 .env.test 就沿用它的密码，否则 ALTER USER 会把密码改掉而文件没同步，测试当场连不上
  TEST_DB_PASSWORD="$(sed -n 's/^DB_PASSWORD=//p' .env.test | head -1)"
fi
mysql_admin <<SQL
create database if not exists \`${DB_NAME}\`
  default character set utf8mb4 collate utf8mb4_bin;
create database if not exists \`${TEST_DB_NAME}\`
  default character set utf8mb4 collate utf8mb4_bin;
create user if not exists '${DB_USER}'@'%' identified by '${DB_PASSWORD}';
alter user '${DB_USER}'@'%' identified by '${DB_PASSWORD}';
create user if not exists '${DB_USER}_test'@'%' identified by '${TEST_DB_PASSWORD}';
alter user '${DB_USER}_test'@'%' identified by '${TEST_DB_PASSWORD}';
grant select, insert, update, delete, create, alter, index, references
  on \`${DB_NAME}\`.* to '${DB_USER}'@'%';
-- 测试每次运行都 DROP + 按 schema 重建，所以要 DDL 权限（DROP/CREATE），不只是增删改查
grant select, insert, update, delete, create, alter, index, references, drop
  on \`${TEST_DB_NAME}\`.* to '${DB_USER}_test'@'%';
flush privileges;
SQL

echo "📐 应用表结构 db/schema.mysql.sql → ${DB_NAME}"
docker exec -i "$MYSQL_CONTAINER" \
  mysql -u"$DB_USER" -p"$DB_PASSWORD" --default-character-set=utf8mb4 "$DB_NAME" \
  < db/schema.mysql.sql

echo "🧪 写 ${ROOT_DIR}/.env.test（测试库连接参数，npm test 读它）"
# DB_HOST 这里是 127.0.0.1：测试跑在宿主机上，解析不到容器名 `mysql`
# （报 getaddrinfo ENOTFOUND mysql），而共享容器把 3306 正好发布在回环地址上。
cat > .env.test <<EOF
DB_HOST=127.0.0.1
DB_PORT=3306
DB_NAME=${TEST_DB_NAME}
DB_USER=${DB_USER}_test
DB_PASSWORD=${TEST_DB_PASSWORD}
EOF
chmod 600 .env.test

echo "✅ 完成。${DB_NAME} 的表："
mysql_admin -N -e "select table_name from information_schema.tables where table_schema='${DB_NAME}' order by table_name;"

cat <<EOF

下一步：
  npm install                # 只装 mysql2（测试要在宿主机上连库跑）
  npm test                   # 对 cred_broker_test 跑全套断言
  ./scripts/deploy.sh        # 测试闸门 → 构建 → 起容器 → 探活
  open http://127.0.0.1:${APP_PORT:-8789}/v1/health
  ./scripts/gw-join.sh       # 一次性：让公网域名按 Host 转进本容器

数据已由本机 MySQL 独占（云上 Worker 与 D1 已删除，见 README「数据搬迁（已完成）」）。
EOF
