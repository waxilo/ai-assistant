//! Qoder 的 `COSY` 凭据 —— 「真接管」唯一可能的落点。
//!
//! # 为什么不是「把 Bearer 换成扣费账号的 token」
//!
//! 2026-09-19 实测（国内版 Qoder CN 0.3.3）：客户端的业务接口**根本不发 `Bearer <token>`**，
//! 而是把账号凭据**封进请求头自己**：
//!
//! ```text
//! Authorization: Bearer COSY.<payload_b64>.<md5hex>
//! Cosy-User: <uid>      Cosy-Key: <rsa密文>      Cosy-Date: <unix秒>
//! ```
//!
//! 凭据本体在 payload 里、是**密文**；`Cosy-Key` 是解开它的对称密钥（用内嵌 RSA 公钥加密）。
//! 所以「原样透传」= 永远用客户端登录的那个账号，「换成另一个账号的 `Bearer dt-…`」= 服务端
//! 解不开、直接 `101 Signature invalid`（客户端连模型清单都拉不到，会话起不来）。
//!
//! 唯一出路是**按同一算法重算**：本模块就干这个。
//!
//! # 算法（逆自桌面端 `app.asar` 的 `tUe()` / `vPt()` / `bPt()`）
//!
//! ```text
//! A    = 16 个 ASCII 字符（8 随机字节的 hex；**同时**当 AES 密钥与 IV）—— 必须是文本！
//! info = base64( AES-128-CBC(A, A)( JSON{uid, aid, name, email, security_oauth_token} ) )
//! key  = base64( RSA_PKCS1v15(内嵌公钥, A) )
//! n    = base64( JSON{version:"v1", requestId, info, cosyVersion, ideVersion} )
//! sig  = md5( n \n key \n ts \n body \n path )     // path 剥 query、剥 `/algo` 前缀
//! ```
//!
//! 五处容易踩的坑（都实测过）：
//! - **`uid` 必须是 Qoder 侧的账号 id**（`019eb647-…` 那种），不是本应用内部的
//!   `Account::id`。填错的表现是 `105 Login expired`，看起来像 token 过期、其实完全不是。
//! - **`security_oauth_token` 就是账号的 `dt-…` token**（与「换 Bearer」用的是同一个值）。
//! - **签名里带 body 与 path**，所以只能在「拿到完整请求体」的位置重算，且 path 要剥前缀。
//! - ⚠️ **明文的 JSON 字段顺序也要照抄客户端**（`InfoPlain` / `PayloadPlain` 存在的原因）。
//!   用过 `serde_json::json!` 的都知道它落 `BTreeMap`、按字母序输出；明文一变密文全变，
//!   上游回 `101 Signature invalid`。**别用长度校验这种实现**：错版与对版的密文长度一模一样。
//! - ⚠️ **对称密钥 `A` 必须是 16 个可打印 ASCII 字符**（见 `random_secret`）。服务端把它当
//!   字符串用，裸随机字节会被弄坏 —— 而失败症状与上一条完全一样（`101`）。
//!   定位这四条坑的正确姿势：**固定密钥跑整套头**，把 AES / RSA / 签名三段分开验证。

use base64::Engine as _;
use cbc::cipher::{BlockEncryptMut, KeyIvInit};
use md5::{Digest as _, Md5};
use rand::RngCore;
use rsa::pkcs8::DecodePublicKey;
use rsa::Pkcs1v15Encrypt;

/// 客户端内嵌的 RSA 公钥（`app.asar` 里的 `MPt`）。
///
/// 它是**公开**的：服务端用配对的私钥解开 `Cosy-Key`，所以这把公钥本来就在客户端里裸奔。
/// 换号方（我们）照抄它加密自己的随机密钥即可。
pub const PUBLIC_KEY_PEM: &str = "\
-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc
4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l
6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17
XcW+ML9FoCI6AOvOzwIDAQAB
-----END PUBLIC KEY-----";

/// 重签所需的目标账号身份。
pub struct Identity {
    /// Qoder 侧账号 id（`/api/v3/user/status` 的 `id`）—— **不是** `Account::id`。
    pub uid: String,
    pub name: String,
    pub email: String,
    /// 账号的 `dt-…` token。
    pub token: String,
}

/// 重签结果：四个必须**同时**替换的头。
pub struct Rebuilt {
    pub authorization: String,
    pub user: String,
    pub key: String,
    pub date: String,
}

/// 客户端 `Cosy-*` 头的名字。重签时这几个必须**替换**（不是并列），否则上游看到两个值。
pub fn is_cosy_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "cosy-user" | "cosy-key" | "cosy-date"
    )
}

/// 是不是 COSY 形态的 `Authorization`（`Bearer COSY.…`）。
///
/// 大小写不敏感：客户端固定发 `Bearer`，但 header 的 scheme 本就大小写无关，
/// 判错方向的代价是「换号悄悄不生效」，所以这里放宽。
pub fn is_cosy_authorization(value: &str) -> bool {
    match value.split_once(' ') {
        Some((scheme, rest)) => scheme.eq_ignore_ascii_case("bearer") && rest.starts_with("COSY."),
        None => false,
    }
}

/// 客户端签名用的 path（逆自 `bPt`）：剥协议+主机、剥 query/hash、**剥 `/algo` 前缀**。
///
/// 注意「剥 `/algo`」不是可选项：客户端签名时就已经剥掉了，我们算的是同一个字符串，
/// 少剥/多剥一个字符都会让 md5 对不上（服务端回 `101 Signature invalid`）。
pub fn signing_path(target: &str) -> String {
    let bare = match target.find("://") {
        Some(i) => match target[i + 3..].find('/') {
            Some(p) => &target[i + 3 + p..],
            None => "/",
        },
        None => target,
    };
    let no_query = bare.split('?').next().unwrap_or(bare);
    let no_query = no_query.split('#').next().unwrap_or(no_query);
    match no_query.strip_prefix("/algo") {
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        _ => no_query.to_string(),
    }
}

/// `info` 的明文结构。
///
/// ⚠️ **字段顺序必须与客户端逐字节一致**，所以这里用「结构体 + serde 声明序」而不是
/// `serde_json::json!`：后者落进 `BTreeMap`，会**按字母序**输出
/// （`aid,email,name,security_oauth_token,uid`）→ 明文一变、密文全变，
/// 而上游的表现是 `101 Signature invalid`（看起来像签名算错，其实是凭据解出来不对）。
/// 踩过一次：长度相同（143B）、base64 长度相同（192 字符），骗过了所有长度检查。
#[derive(serde::Serialize)]
struct InfoPlain<'a> {
    uid: &'a str,
    aid: &'a str,
    name: &'a str,
    email: &'a str,
    security_oauth_token: &'a str,
}

/// `n` 里那层 payload 的明文结构 —— 同样**保持客户端的字段顺序**。
#[derive(serde::Serialize)]
struct PayloadPlain<'a> {
    version: &'a str,
    #[serde(rename = "requestId")]
    request_id: &'a str,
    info: &'a str,
    #[serde(rename = "cosyVersion")]
    cosy_version: &'a str,
    #[serde(rename = "ideVersion")]
    ide_version: &'a str,
}

/// 生成 `info` 的明文 JSON（客户端顺序）。探针与正式路径共用，避免两处各写一份漂移。
fn info_plaintext(id: &Identity) -> String {
    serde_json::to_string(&InfoPlain {
        uid: &id.uid,
        aid: "",
        name: &id.name,
        email: &id.email,
        security_oauth_token: &id.token,
    })
    .unwrap_or_default()
}

/// 用目标账号身份重算一整套 COSY 头。
///
/// 返回 `None` = 这个请求不满足重签条件（原 Authorization 不是 COSY、body 不是 UTF-8 等）。
/// 调用方在 `None` 时应当**原样透传**，绝不半改。
pub fn rebuild(
    orig_authorization: &str,
    id: &Identity,
    target: &str,
    body: &[u8],
    now_secs: i64,
) -> Option<Rebuilt> {
    rebuild_with_secret(orig_authorization, id, target, body, now_secs, random_secret())
}

/// 生成对称密钥 `A`：**16 个 ASCII 字符**（8 随机字节的 hex），既是 AES-128 密钥也是 IV。
///
/// ⚠️ 绝不能直接拿 16 个随机**字节**来当密钥。服务端是把 RSA 解开的那 16 字节当**字符串**
/// 用的，非 UTF-8 的随机字节会被弄坏 → AES 解不出凭据 → 上游回 `101 Signature invalid`
/// （**签名算式本身完全正确**，所以这一步极易误判成「AES 实现有 bug」）。
/// 实测：任意二进制 16B 必挂；任意可打印 ASCII 16 字符（连 `7f7f7f7f…` 这种都行）全过。
/// 官方客户端给的也是文本密钥，这里照抄 hex 形态。
fn random_secret() -> [u8; 16] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut raw = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut raw);
    let mut secret = [0u8; 16];
    for (i, b) in raw.iter().enumerate() {
        secret[i * 2] = HEX[(b >> 4) as usize];
        secret[i * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
    secret
}

/// 与 `rebuild` 同一套算法，但对称密钥由调用方给定。
///
/// 存在的理由只有一个：**让探针用固定密钥复现整套头**，好跟 JS 版做三段（AES / RSA / 签名）
/// 分离定位。随机密钥下只能看到「整包头不行」，分不清是哪一段错。
pub fn rebuild_with_secret(
    orig_authorization: &str,
    id: &Identity,
    target: &str,
    body: &[u8],
    now_secs: i64,
    secret: [u8; 16],
) -> Option<Rebuilt> {
    let rest = orig_authorization.split_once(' ')?.1;
    let rest = rest.strip_prefix("COSY.")?;
    // `COSY.<payload_b64>.<sig>`；base64 字母表里没有 `.`，所以 split 安全。
    let (payload_b64, _orig_sig) = rest.split_once('.')?;
    let payload: serde_json::Value =
        serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(payload_b64).ok()?)
            .ok()?;

    // 保留客户端的环境特征（版本号之类），只替换凭据。
    let version = payload
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("v1");
    let cosy_version = payload
        .get("cosyVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("1.0.0");
    let ide_version = payload.get("ideVersion").and_then(|v| v.as_str()).unwrap_or("");
    let request_id = match payload.get("requestId").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => uuid::Uuid::new_v4().simple().to_string(),
    };

    let body_str = std::str::from_utf8(body).ok()?;

    // ② 凭据封进 info（AES-128-CBC / PKCS#7）—— 明文顺序见 `InfoPlain`
    let info_json = info_plaintext(id);
    let info = cbc::Encryptor::<aes::Aes128>::new_from_slices(&secret, &secret)
        .ok()?
        .encrypt_padded_vec_mut::<block_padding::Pkcs7>(info_json.as_bytes());
    let info_b64 = base64::engine::general_purpose::STANDARD.encode(info);

    // ③ 对称密钥用内嵌公钥加密 → Cosy-Key
    let public = rsa::RsaPublicKey::from_public_key_pem(PUBLIC_KEY_PEM).ok()?;
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(
        public
            .encrypt(&mut rand::thread_rng(), Pkcs1v15Encrypt, &secret)
            .ok()?,
    );

    // ④ 组 payload + md5 签名（payload 的字段顺序同样照抄客户端）
    let payload_new = serde_json::to_string(&PayloadPlain {
        version,
        request_id: &request_id,
        info: &info_b64,
        cosy_version,
        ide_version,
    })
    .ok()?;
    let n = base64::engine::general_purpose::STANDARD.encode(payload_new.as_bytes());
    let sig_input = format!(
        "{}\n{}\n{}\n{}\n{}",
        n,
        key_b64,
        now_secs,
        body_str,
        signing_path(target)
    );
    let sig = format!("{:x}", Md5::digest(sig_input.as_bytes()));

    Some(Rebuilt {
        authorization: format!("Bearer COSY.{n}.{sig}"),
        user: id.uid.clone(),
        key: key_b64,
        date: now_secs.to_string(),
    })
}

/// 用账号 token 取 COSY 需要的 uid（`/api/v3/user/status` 的 `id`）。
///
/// `base` 传**接管区域的网关**（国内版 `gateway.qoder.com.cn`）—— 与业务请求同域，
/// 避免跨域拿到两套账号 id。
pub async fn fetch_uid(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> Option<String> {
    let url = format!("{}/api/v3/user/status", base.trim_end_matches('/'));
    let resp = client.get(&url).bearer_auth(token).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("id")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_cosy_authorization() {
        assert!(is_cosy_authorization("Bearer COSY.eyJhIjoxfQ.abc"));
        assert!(is_cosy_authorization("bearer COSY.x.y"));
        // 普通账号凭证不是 COSY，绝不能走重签
        assert!(!is_cosy_authorization("Bearer dt-abcdefg"));
        assert!(!is_cosy_authorization("Signature 1a2b3c"));
        assert!(!is_cosy_authorization(""));
        assert!(!is_cosy_authorization("COSY.abc.def"));
    }

    #[test]
    fn recognises_cosy_headers() {
        assert!(is_cosy_header("Cosy-User"));
        assert!(is_cosy_header("cosy-key"));
        assert!(is_cosy_header("COSY-DATE"));
        assert!(!is_cosy_header("Authorization"));
        assert!(!is_cosy_header("X-Client-Timestamp"));
    }

    #[test]
    fn signing_path_strips_query_and_algo_prefix() {
        // 客户端签名时用的是「剥掉 /algo 前缀、剥掉 query」的 path
        assert_eq!(signing_path("/algo/api/v2/model/list?Encode=1"), "/api/v2/model/list");
        assert_eq!(
            signing_path("/algo/api/v2/service/pro/sse/agent_chat_generation"),
            "/api/v2/service/pro/sse/agent_chat_generation"
        );
        // 没有 /algo 前缀的不动
        assert_eq!(signing_path("/api/v3/user/status"), "/api/v3/user/status");
        // 绝对 URL 也支持
        assert_eq!(
            signing_path("https://gateway.qoder.com.cn/algo/api/v2/model/list?a=1"),
            "/api/v2/model/list"
        );
        // 别把 "/algorithm" 这类前缀误伤
        assert_eq!(signing_path("/algorithm/x"), "/algorithm/x");
    }

    fn identity() -> Identity {
        Identity {
            uid: "019eb647-a8b6-7664-ac01-5a9201eec888".to_string(),
            name: "nick4300340010".to_string(),
            email: String::new(),
            token: "dt-abcdefghijklmnopqrstuvwxy".to_string(),
        }
    }

    fn orig_authorization() -> String {
        let payload = serde_json::json!({
            "version": "v1",
            "requestId": "0123456789abcdef0123456789abcdef",
            "info": "QUJD",
            "cosyVersion": "1.0.0",
            "ideVersion": "0.3.3",
        })
        .to_string();
        let n = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
        format!("Bearer COSY.{n}.deadbeef")
    }

    /// 密钥必须是 16 个可打印 ASCII 字符 —— 上游把它当字符串，二进制会被弄坏（`101`）。
    #[test]
    fn secret_is_printable_ascii() {
        for _ in 0..128 {
            let s = random_secret();
            assert!(
                s.iter().all(|b| (0x21..=0x7e).contains(b)),
                "密钥必须全是可打印 ASCII：{:?}",
                String::from_utf8_lossy(&s)
            );
        }
        // 也不该退化成常量
        assert_ne!(random_secret(), random_secret());
    }

    /// 明文必须与客户端**逐字节**一致：字段顺序错了上游只回 `101 Signature invalid`，
    /// 长度（密文 143B→144B、base64 192 字符）却与正确实现完全相同，肉眼查不出来。
    #[test]
    fn plaintext_key_order_matches_client() {
        let id = identity();
        assert_eq!(
            info_plaintext(&id),
            r#"{"uid":"019eb647-a8b6-7664-ac01-5a9201eec888","aid":"","name":"nick4300340010","email":"","security_oauth_token":"dt-abcdefghijklmnopqrstuvwxy"}"#
        );
        let p = serde_json::to_string(&PayloadPlain {
            version: "v1",
            request_id: "r",
            info: "I",
            cosy_version: "1.0.0",
            ide_version: "0.3.3",
        })
        .unwrap();
        assert_eq!(
            p,
            r#"{"version":"v1","requestId":"r","info":"I","cosyVersion":"1.0.0","ideVersion":"0.3.3"}"#
        );
    }

    #[test]
    fn rebuild_keeps_client_env_and_swaps_credential() {
        let orig = orig_authorization();
        let out = rebuild(
            &orig,
            &identity(),
            "/algo/api/v2/model/list?Encode=1",
            b"",
            1_789_800_000,
        )
        .expect("应能重签");

        // 换号落点：Cosy-User 必须是目标账号的 uid
        assert_eq!(out.user, "019eb647-a8b6-7664-ac01-5a9201eec888");
        assert_eq!(out.date, "1789800000");

        let rest = out.authorization.strip_prefix("Bearer COSY.").unwrap();
        let (payload_b64, sig) = rest.split_once('.').unwrap();
        assert_eq!(sig.len(), 32, "签名是 md5 hex");
        assert_eq!(out.authorization.len(), rest.len() + "Bearer COSY.".len());

        // 环境特征保留、凭据替换
        let payload: serde_json::Value =
            serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(payload_b64).unwrap()).unwrap();
        assert_eq!(payload["ideVersion"], "0.3.3");
        assert_eq!(payload["cosyVersion"], "1.0.0");
        assert_eq!(payload["requestId"], "0123456789abcdef0123456789abcdef");
        assert_ne!(payload["info"], "QUJD", "info 必须是重算出来的密文");
        // 1024 位公钥 → PKCS#1 v1.5 密文固定 128 字节
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(out.key.as_bytes())
                .unwrap()
                .len(),
            128
        );
    }

    #[test]
    fn rebuild_signs_body_and_path() {
        let orig = orig_authorization();
        let a = rebuild(&orig, &identity(), "/algo/api/v2/x", b"{\"a\":1}", 1).unwrap();
        let b = rebuild(&orig, &identity(), "/algo/api/v2/x", b"{\"a\":2}", 1).unwrap();
        let c = rebuild(&orig, &identity(), "/algo/api/v2/y", b"{\"a\":1}", 1).unwrap();
        // 签名覆盖 body 与 path ⇒ 任一不同，签名必不同
        assert_ne!(a.authorization, b.authorization);
        assert_ne!(a.authorization, c.authorization);
    }

    #[test]
    fn rebuild_refuses_non_cosy_and_non_utf8() {
        let id = identity();
        assert!(rebuild("Bearer dt-abc", &id, "/x", b"", 1).is_none());
        assert!(rebuild("Signature 1a2b", &id, "/x", b"", 1).is_none());
        // body 不是 UTF-8：拒绝重签（宁可原样透传，也不能签一个错的）
        assert!(rebuild(&orig_authorization(), &id, "/x", &[0xff, 0xfe], 1).is_none());
    }

    /// 真机探针（默认 `#[ignore]`）：把本模块算出来的头打到**真实上游**，
    /// 用来把「重签实现错了」与「转发环节把头发坏了」分开。
    ///
    /// ```bash
    /// cargo test --lib -- --ignored --nocapture cosy_probe
    /// ```
    #[test]
    #[ignore = "需要联网与本机账号，手动跑"]
    fn cosy_probe_prints_headers() {
        let home = std::env::var("HOME").unwrap();
        let raw = std::fs::read_to_string(format!(
            "{home}/Library/Application Support/com.waxilo.qoder-assistant/accounts.json"
        ))
        .unwrap();
        let items: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
        let a = items
            .iter()
            .find(|x| x["name"] == "nick0494015252")
            .expect("找不到探针账号");
        let id = Identity {
            uid: "01a0b818-0f1b-7b57-b9c3-b4eab27d8e93".to_string(),
            name: a["name"].as_str().unwrap().to_string(),
            email: String::new(),
            token: a["token"].as_str().unwrap().to_string(),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let info_json = info_plaintext(&id);
        println!("PROBE_INFO_LEN={}", info_json.len());
        println!("PROBE_INFO={}", info_json);
        println!("PROBE_UID_LEN={}", id.uid.len());
        println!("PROBE_TOKEN_LEN={}", id.token.len());

        // 固定密钥下的**整套头**：与 JS 版交叉验证用。
        // 随机密钥看不到现场，固定密钥才能把 AES / RSA / 签名三段拆开定位。
        let fixed = *b"0123456789abcdef";
        println!(
            "PROBE_SECRET_HEX={}",
            fixed.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let target = "/algo/api/v2/config/getDataPolicy?requestId=probe&version=2";
        let fx = rebuild_with_secret(&orig_authorization(), &id, target, b"", now, fixed)
            .expect("固定密钥重签应成功");
        println!("PROBE_AUTH_FIXED={}", fx.authorization);
        println!("PROBE_KEY_FIXED={}", fx.key);
        println!("PROBE_DATE_FIXED={}", fx.date);
        println!(
            "PROBE_INFO_FIXED={}",
            base64::engine::general_purpose::STANDARD.encode(
                cbc::Encryptor::<aes::Aes128>::new_from_slices(&fixed, &fixed)
                    .unwrap()
                    .encrypt_padded_vec_mut::<block_padding::Pkcs7>(info_json.as_bytes())
            )
        );

        let out = rebuild(&orig_authorization(), &id, target, b"", now).expect("重签应成功");
        println!("PROBE_AUTH={}", out.authorization);
        println!("PROBE_USER={}", out.user);
        println!("PROBE_KEY={}", out.key);
        println!("PROBE_DATE={}", out.date);
    }
}
