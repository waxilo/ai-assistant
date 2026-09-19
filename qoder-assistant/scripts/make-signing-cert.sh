#!/usr/bin/env bash
#
# 生成 QoderAssistant 的稳定代码签名身份，并导入登录钥匙串。
#
# 为什么需要它：Qoder 助手的「智能接管」要往**别的应用**的包里写补丁（Qoder CN.app 的
# worker 产物），这受 macOS 的「App 管理」（TCC）管辖；而 TCC 把授权记在**代码身份**上：
#
#   未签名 / ad-hoc  → designated requirement 只有 `cdhash H"…"`（QoderAssistant 是 Tauri
#                      默认的 adhoc,linker-signed，标识符甚至是 `qoder_assistant-<hash>`）
#                      → 二进制一变（重新构建 / 自动更新）身份就失配，系统把它当成新应用。
#                      实测表现是**静默 EPERM**：连权限弹窗都没有，用户看到的是「接管没生效」。
#   固定证书签名     → DR 变成 `identifier "…" and certificate root = H"…"`，重建多少次都匹配。
#
# 证书自签即可，不需要 Apple 开发者账号，也不需要公证 —— 系统只要求「身份稳定」。
#
# 两种模式：
#   A) 自带 CA（默认）：新生成一套 CA + 叶证书。
#      ⚠️ 新 CA 要额外授权一次「信任设置」（会弹系统对话框），且重复执行等于换身份。
#   B) 共用已有 CA：CA_DIR=<已有凭据目录>。叶证书由**已受信任的** CA 签发，
#      导入后立即可用、**不需要任何新的信任授权**，也不多一个信任根。
#      TCC 的 designated requirement 锚的是**根证书哈希**，所以共用 CA 完全没问题 ——
#      而 `identifier` 不同，两个应用在 TCC 里各记各的条目、互不干扰。
#      本机现状就是这么配的（见 qoder-assistant/README.md）。
#
# 用法：
#   bash scripts/make-signing-cert.sh                                  # A) 新 CA
#   CA_DIR="${HOME}/.traework-signing" bash scripts/make-signing-cert.sh   # B) 共用 CA
#
# 产物（默认 ~/.qoder-signing，权限 700，**绝不要提交进 git**）：
#   ca.crt                 根证书（A 模式新生成；B 模式是 CA_DIR 那份的副本，只为留档）
#   ca.key                 根私钥（仅 A 模式；B 模式的锚点在 CA_DIR，不复制私钥）
#   leaf.crt / leaf.key    代码签名叶证书
#   leaf.p12               打包版（叶证书 + 私钥 + 根 CA），供 CI 导入
#   ci-cert-p12.b64        p12 的 base64 → GitHub Secret `MACOS_CERT_P12_QODER`
#   ci-cert-password.txt   p12 密码     → GitHub Secret `MACOS_CERT_PASSWORD_QODER`

set -euo pipefail

# 注意：所有变量展开都写成 ${VAR}。本脚本的提示语里中文紧跟变量（如「${VAR}）」），
# 而 UTF-8 全角字符的字节会被 bash 当成标识符字符 → 不加大括号就会把中文吃进变量名。
CN_CA="${CN_CA:-QoderAssistant Local CA}"
CN_LEAF="${CN_LEAF:-QoderAssistant Self-Signed}"
DAYS="${DAYS:-3650}"
CA_DIR="${CA_DIR:-}"
OUT_DIR="${OUT_DIR:-${HOME}/.qoder-signing}"
KEYCHAIN="${KEYCHAIN:-${HOME}/Library/Keychains/login.keychain-db}"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
cd "${WORK}"

if [ -n "${CA_DIR}" ]; then
  echo "==> 共用 CA 模式：CA 取自 ${CA_DIR}"
  for f in ca.crt ca.key; do
    [ -f "${CA_DIR}/${f}" ] || { echo "✗ ${CA_DIR} 里缺 ${f}"; exit 1; }
  done
  cp "${CA_DIR}/ca.crt" "${CA_DIR}/ca.key" .
  CN_CA="$(openssl x509 -in ca.crt -noout -subject | sed 's/.*CN *= *//; s/,.*//')"
  echo "    根证书 CN：${CN_CA}"
  echo "    根证书指纹：$(openssl x509 -in ca.crt -noout -fingerprint -sha1 | sed 's/.*=//')"
  echo "    （这个哈希就是 TCC 授权里的 certificate root —— 换 CA 等于换身份）"
else
  echo "==> 1/5 生成自签根 CA（${CN_CA}）"
  openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days "${DAYS}" \
    -keyout ca.key -out ca.crt \
    -subj "/CN=${CN_CA}/O=QoderAssistant" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign"
fi

echo "==> 生成叶证书密钥与 CSR（${CN_LEAF}）"
openssl req -newkey rsa:2048 -nodes -sha256 \
  -keyout leaf.key -out leaf.csr \
  -subj "/CN=${CN_LEAF}/O=QoderAssistant"

cat > leaf.ext <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
EOF

echo "==> 用 CA 签发叶证书"
openssl x509 -req -in leaf.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out leaf.crt -days "${DAYS}" -sha256 -extfile leaf.ext

echo "==> 导入登录钥匙串"
# 同名的旧身份必须先清掉，否则 codesign 会报 ambiguous（同时命中两个）。
if security find-identity -p codesigning "${KEYCHAIN}" 2>/dev/null | grep -qF "${CN_LEAF}"; then
  echo "    清掉同名的旧身份（重新签发后不能并存）"
  security delete-identity -c "${CN_LEAF}" "${KEYCHAIN}" >/dev/null 2>&1 || true
fi
# 共用 CA 时根证书通常已在钥匙串里，重复导入会报 "already exists" —— 那是正常情况，不当失败。
import_item() {  # import_item <file> [额外的 security import 参数...]
  local file="$1"; shift
  local out
  if out="$(security import "${file}" -k "${KEYCHAIN}" "$@" 2>&1)"; then
    return 0
  fi
  case "${out}" in
    *"already exists"*) echo "    已存在，跳过：$(basename "${file}")" ;;
    *) echo "✗ 导入 $(basename "${file}") 失败：${out}" >&2; return 1 ;;
  esac
}

import_item ca.crt -T /usr/bin/codesign -T /usr/bin/security
# -A：允许任意程序使用这把私钥且**不弹框**。少了它会卡在「codesign 想要使用钥匙串中的密钥」，
# 在无 GUI 的构建（CI / 脚本）里表现成**挂住不动**。
import_item leaf.key -T /usr/bin/codesign -T /usr/bin/security -A -P ''
import_item leaf.crt -T /usr/bin/codesign -T /usr/bin/security

if [ -z "${CA_DIR}" ]; then
  echo "==> 设置信任根（会弹一次系统对话框，属正常）"
  security add-trusted-cert -r trustRoot -p codeSign -k "${KEYCHAIN}" ca.crt
else
  echo "==> 信任根：沿用已有设置（共用 CA），无需重新授权"
fi

echo "==> 导出 CI 凭据"
mkdir -p "${OUT_DIR}"
PASS="$(openssl rand -hex 20)"
PASS="${PASS:0:28}"
openssl pkcs12 -export -out leaf.p12 -inkey leaf.key -in leaf.crt \
  -certfile ca.crt -passout "pass:${PASS}"

cp ca.crt leaf.crt leaf.key leaf.p12 "${OUT_DIR}/"
if [ -z "${CA_DIR}" ]; then
  cp ca.key "${OUT_DIR}/"
else
  # 共用 CA 时**绝不在本目录留私钥副本**：锚点只能有一个家（CA_DIR）。
  # 留一份不同步的 ca.key 会让「以后用 CA_DIR=这里 重签叶证书」悄悄签出坏链。
  rm -f "${OUT_DIR}/ca.key"
  echo "    锚点私钥留在 ${CA_DIR}/ca.key，本目录不复制"
fi
openssl base64 -A -in leaf.p12 > "${OUT_DIR}/ci-cert-p12.b64"
printf '%s' "${PASS}" > "${OUT_DIR}/ci-cert-password.txt"
chmod 700 "${OUT_DIR}"
chmod 600 "${OUT_DIR}"/*

echo
if security find-identity -v -p codesigning "${KEYCHAIN}" | grep -F "${CN_LEAF}"; then
  echo "✓ 签名身份已就绪：${CN_LEAF}"
else
  echo "✗ 身份仍不可用（多半是根证书没被信任）。手动确认："
  echo "    打开「钥匙串访问」→ 找 ${CN_CA} → 双击 → 信任 → 代码签名选「始终信任」"
  echo "  或换个已经受信任的 CA：CA_DIR=<那个凭据目录> bash $0"
  exit 1
fi

echo
echo "--- 自检：真的能用它签一个临时文件吗 ---"
cp /bin/echo "${WORK}/signtest"
if codesign -f -s "${CN_LEAF}" "${WORK}/signtest" 2>&1; then
  codesign -dvv "${WORK}/signtest" 2>&1 | grep -E 'Identifier|Authority' | sed 's/^/    /'
  echo "    DR: $(codesign -d -r- "${WORK}/signtest" 2>&1 | sed -n 's/^# designated => //p')"
else
  echo "✗ 签名自检失败 —— 上面的报错就是原因"
  exit 1
fi

echo
echo "凭据已写入 ${OUT_DIR}（权限 600，请勿提交进 git）："
ls -1 "${OUT_DIR}"
echo
echo "接下来："
echo "  1) GitHub Secrets（仓库级，两个应用各自一套）："
echo "       MACOS_CERT_P12_QODER      ← ${OUT_DIR}/ci-cert-p12.b64 的全部内容"
echo "       MACOS_CERT_PASSWORD_QODER ← ${OUT_DIR}/ci-cert-password.txt 的内容"
echo "       gh secret set MACOS_CERT_P12_QODER      --repo <owner>/<repo> < ${OUT_DIR}/ci-cert-p12.b64"
echo "       gh secret set MACOS_CERT_PASSWORD_QODER --repo <owner>/<repo> < ${OUT_DIR}/ci-cert-password.txt"
echo "  2) 重新构建（npm run build:local）—— tauri.conf.json 里已写死身份，构建自动签。"
echo
echo "换机器时：把 CA_DIR 与 ${OUT_DIR} 都拷过去，重跑「导入 + 信任 CA」两步，**不要重新生成** —— 新 CA 会换掉身份。"
