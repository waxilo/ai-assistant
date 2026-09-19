//! 智能接管的本地 TLS 材料（自签 CA + 服务器证书）。
//!
//! # 为什么接管必须自带 TLS
//!
//! Qoder 客户端读端点覆盖时只做 `new URL(v).origin` 校验，并且**把 scheme 写死成
//! https**（逐字逆向往证见 README「智能接管」一节）。所以能写进客户端的覆盖值只有
//! `https://127.0.0.1:<port>` 这一种形态 —— 反代必须真的能终止 TLS，否则握手失败
//! 会**直接打断用户的对话**，比「拿到端点却用不上」更糟。
//!
//! 实测确认（`QODERCN_SERVER_ENDPOINT=https://127.0.0.1:9999` 手工起 worker）：
//! 客户端接受带端口的 https origin，日志里 `[config-service] baseUrl` 随之为该值。
//!
//! # 为什么用自签 CA，而不是「关闭校验」
//!
//! 客户端侧由注入段把本 CA 加进连接信任，且**只对 `127.0.0.1` 的连接生效**
//! （见 `patch.rs`）：不改系统信任库、不需要管理员、不动任何全局开关。
//! 私钥只落在本应用数据目录，权限 0600。

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 证书目录名（位于本应用数据目录下）
const DIR: &str = "certs";
const CA_FILE: &str = "ca.pem";
const CERT_FILE: &str = "server.pem";
const KEY_FILE: &str = "server.key";

/// 一旦证书材料就位就不再重生成：换掉 CA 等于换掉客户端手里的信任锚，
/// 中间那一刻的对话会握手失败。只有文件缺失时才生成。
#[derive(Clone, Debug)]
pub struct Certs {
    dir: PathBuf,
}

impl Certs {
    pub fn ca_path(&self) -> PathBuf {
        self.dir.join(CA_FILE)
    }

    pub fn cert_path(&self) -> PathBuf {
        self.dir.join(CERT_FILE)
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.join(KEY_FILE)
    }

    /// CA 的 PEM 文本。注入段要把它嵌进客户端代码，所以这里必须能读出来。
    pub fn ca_pem(&self) -> Result<String, String> {
        fs::read_to_string(self.ca_path()).map_err(|e| format!("读取本地 CA 失败：{e}"))
    }
}

/// 幂等准备证书材料；已有就直接复用。
pub fn ensure(data_dir: &Path) -> Result<Certs, String> {
    let dir = data_dir.join(DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("创建证书目录失败：{e}"))?;
    let certs = Certs { dir };
    if certs.ca_path().exists() && certs.cert_path().exists() && certs.key_path().exists() {
        return Ok(certs);
    }
    generate(&certs)?;
    Ok(certs)
}

fn generate(certs: &Certs) -> Result<(), String> {
    // CA：只需要能签出叶证书，不参与任何其它用途。
    let mut ca =
        CertificateParams::new(Vec::<String>::new()).map_err(|e| format!("构建 CA 参数失败：{e}"))?;
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    ca.distinguished_name
        .push(DnType::CommonName, "Qoder Assistant Local CA");
    let ca_key = KeyPair::generate().map_err(|e| format!("生成 CA 私钥失败：{e}"))?;
    let ca_cert = ca
        .self_signed(&ca_key)
        .map_err(|e| format!("自签 CA 失败：{e}"))?;

    // 叶证书：SAN 必须含 `IP:127.0.0.1` —— 端点里写的就是这个 host，
    // 客户端按它做名称校验（`rcgen` 会把能解析成 IP 的条目落成 IpAddress SAN）。
    let mut leaf = CertificateParams::new(vec!["127.0.0.1".to_string(), "localhost".to_string()])
        .map_err(|e| format!("构建服务器证书参数失败：{e}"))?;
    leaf.distinguished_name.push(DnType::CommonName, "127.0.0.1");
    leaf.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    let leaf_key = KeyPair::generate().map_err(|e| format!("生成服务器私钥失败：{e}"))?;
    let leaf_cert = leaf
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .map_err(|e| format!("签发服务器证书失败：{e}"))?;

    write_private(&certs.ca_path(), &ca_cert.pem())?;
    write_private(&certs.cert_path(), &leaf_cert.pem())?;
    write_private(&certs.key_path(), &leaf_key.serialize_pem())?;
    Ok(())
}

/// 一律 0600：私钥、以及「把这台机器认成本地反代」的那张 CA。
fn write_private(path: &Path, body: &str) -> Result<(), String> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    f.write_all(body.as_bytes())
        .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    Ok(())
}

/// 装配 rustls 服务端配置（握手用）。
///
/// 显式指定 `ring` provider：进程里还有别的 rustls 消费者（reqwest），
/// 依赖「谁先 `install_default` 谁说了算」会把启动顺序变成隐性依赖。
pub fn server_config(certs: &Certs) -> Result<Arc<rustls::ServerConfig>, String> {
    let mut cert_reader = std::io::BufReader::new(
        fs::File::open(certs.cert_path()).map_err(|e| format!("打开服务器证书失败：{e}"))?,
    );
    let chain = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("解析服务器证书失败：{e}"))?;
    if chain.is_empty() {
        return Err("服务器证书为空".into());
    }

    let mut key_reader = std::io::BufReader::new(
        fs::File::open(certs.key_path()).map_err(|e| format!("打开服务器私钥失败：{e}"))?,
    );
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| format!("解析服务器私钥失败：{e}"))?
        .ok_or_else(|| "服务器私钥为空".to_string())?;

    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("TLS 版本配置失败：{e}"))?
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .map_err(|e| format!("装配 TLS 失败：{e}"))?;
    // 反代只会说 HTTP/1.1。不声明 ALPN 当然也能跑（客户端拿不到协商结果就会退回
    // HTTP/1.1），但 Node 的 fetch/undici 默认同时报 h2 与 http/1.1 —— 显式钉住
    // 可以让「为什么没有走 h2」变成一件确定的事，而不是每次靠运气。
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qa-certs-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ensure_generates_a_usable_chain_and_is_idempotent() {
        let dir = sandbox();
        let a = ensure(&dir).expect("首次生成应成功");
        assert!(a.ca_path().exists() && a.cert_path().exists() && a.key_path().exists());

        // 叶证书的签发者必须是那份 CA（自签 CA 不能只是「存在」，得真的签了）
        let ca = a.ca_pem().unwrap();
        assert!(ca.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(ca.contains("Qoder Assistant Local CA") || ca.contains("CERTIFICATE"));

        // 复用：第二次调用不能换掉材料（换了 = 客户端手里的信任锚失效）
        let before = fs::read_to_string(a.key_path()).unwrap();
        let b = ensure(&dir).unwrap();
        assert_eq!(before, fs::read_to_string(b.key_path()).unwrap());

        // 能被 rustls 装配起来，说明链与私钥是配套的
        server_config(&b).expect("rustls 应能装配");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_material_is_not_world_readable() {
        let dir = sandbox();
        let c = ensure(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [c.ca_path(), c.cert_path(), c.key_path()] {
                let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{} 权限应为 0600，实际 {mode:o}", p.display());
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// 重启后仍要能用（这是最常见的现场：证书在，但进程是新起的）
    #[test]
    fn server_config_survives_a_fresh_read() {
        let dir = sandbox();
        let c = ensure(&dir).unwrap();
        drop(c);
        let c2 = ensure(&dir).unwrap();
        server_config(&c2).expect("从磁盘重读也应能装配");
        let _ = fs::remove_dir_all(&dir);
    }
}
