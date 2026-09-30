//! 端点注入补丁：让 Qoder 客户端的对话请求**真的**走本机反代。
//!
//! # 为什么是打补丁，而不是写配置
//!
//! 客户端 SDK 起推理进程时，环境变量的来源是
//! `buildEnv(){ let e = this.options.env ?? {...process.env} }` —— 也就是说
//! **配置文件里的 `env` 块没有任何消费者**（`app.asar` 与 worker 产物全量扫描均无）。
//! 而进程 env 由桌面端在 spawn 那一刻构造，本应用够不着。
//!
//! 唯一既不改系统、也不动客户端的落点，就是**被执行的 worker 产物本身**：
//!
//! ```text
//! <app>/Contents/Resources/app.asar.unpacked/node_modules/@qoder-ai/
//!   qoder-{cn-,}agent-sdk/dist/_worker/qoder-worker-runtime.obf.mjs
//! ```
//!
//! 它在 asar **之外**（不受归档完整性校验约束）。
//!
//! # 但**改完要重启客户端**（2026-09-21 修正）—— 已再次被推翻（2026-09-24 终审）
//!
//! 这一节的历史完整记下来，因为这个判断反转了两次，第三次才有实验钉死：
//!
//! - **2026-09-21 之前**：以为「客户端每次会话都重新起进程执行产物，改完即生效」；
//! - **2026-09-21**：真机观察「开接管不重启，对话仍直连」，于是改成「桌面端启动时
//!   读一次、之后用内存副本，必须重启」，并为此造了整套进程收尾（`client_proc`）；
//! - **2026-09-24**：惰性标记活体实验（给产物注入一段只写日志的代码 → 用户不重启
//!   直接发对话 → 标记命中）证明 **0.3.4 起的客户端每次会话都新起 worker 线程、
//!   从磁盘重读产物**，CN / 国际两个变体都不例外 —— 09-21 的「必须重启」描述的是
//!   旧版（≤0.3.3）独立长驻子进程架构，该架构已经不存在，`client_proc` 据此整体删除。
//!
//! 所以上面第 1 节里「客户端 SDK 起推理进程时 env 由桌面端构造」仍然成立，落点仍然
//! 只能是产物文件本身；变的只是「改动何时被读到」：**下一次对话**，无需重启。
//!
//! # 注入段：每个区域走它自己那条路（[`Region::takeover`]）
//!
//! **国内版**：`process.env.QODERCN_SERVER_ENDPOINT = <本机反代>`。
//! 客户端读端点的唯一入口是 `v7a(){ ... process.env[aue] ... }`，
//! 其中 `aue = Rr("SERVER_ENDPOINT")`、`Rr(name) = ${prefix}${name}`、
//! CN 构建的前缀硬编码为 `QODERCN_` —— 所以键名在源码里**没有字面量**，
//! 只能顺着 `Rr=` 回溯。已用真实进程实测：该键一写进 env，
//! 客户端日志的 `[config-service] baseUrl` 立刻变成该值。
//!
//! **国际版**：没有可读的键（`v7a()` 那句 `if(!Ja) return` 在国际版构建里恒不生效），
//! 所以注入段改在 `tls.connect` 里把**模型网关族**
//! （`api*.qoder.sh` / `api*.qoder.com.cn`，与 [`crate::region::is_model_gateway_host`]
//! 同一条规则）的连接目标改写成 `127.0.0.1:<port>`，并**保留 SNI** ——
//! 反代握手后按 SNI 选上游。业务面（center / openapi）不在族内，原样直连。
//!
//! 判「是不是族内连接」要**连 `servername` 一起测**，不能只看 host：httpdns 模式
//! （对话请求全走这条）下，客户端自建的 resolved-ip Agent 会把 `hostname` 换成
//! 解析出的 IP、只把域名留在 `servername` 里 —— 2026-09-29 真机踩过「接管全绿、
//! 对话零条」，就是只测 host 漏掉了这一整类连接。两个字段任一命中就改写；
//! 改写时 servername 已有值就原样保留，域名那半边才是给反代的 SNI。
//!
//! **两条路都要**把本机 CA 注入 `node:tls`，**只对回环地址的连接**：
//! 端点被客户端强制成 https（`M7a()` 只接受 `https:` origin），所以反代终止 TLS
//! 后必须让客户端认这张自签证书 —— 走注入而不是改系统信任库，因此不需要管理员。
//!
//! # 官方更新会覆盖它
//!
//! 每次启用与心跳都会比对文件头：不是我们的注入段就重新打一遍
//! （先把当前这份官方原版存成备份）。**比对的不只是 url/CA，还包括注入体指纹**
//! （标记行里的 `js=`）—— 只比前两者的话，改了本文件注入段代码、而 url/CA 没变时，
//! 盘上的旧注入会被当成「已是最新」，心跳永远不重写（2026-09-29 实遇：dev 重编译
//! 出带新判据的构建，产物里却还是旧注入）。旧格式 marker 没有 `js=` → 自动判过时
//! → 下一次心跳重写（升级自愈，不用手动关开接管）。
//! 摘除是**精确剥离**，不依赖备份回滚 —— 备份只作额外保险，避免「用旧版客户端
//! 覆盖掉用户刚更新到的新版」。

use crate::region::{Region, Takeover};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// 注入段的开标记。**故意不带结尾的 `*/`**：真正的行是
/// `/*qoder-assistant-takeover:begin url=… ca=… */`，参数就写在注释里，
/// 这样「是否已注入 / 注入了什么」只读文件头 4KB 就能判定，不必执行 JS。
const MARK_BEGIN: &str = "/*qoder-assistant-takeover:begin";
const MARK_END: &str = "/*qoder-assistant-takeover:end*/";

/// 判定是否已注入时最多读多少字节（注入段本身 ≈ CA 的 PEM 长度，约 1.5KB）
const HEAD_BYTES: usize = 8192;

/// worker 产物的候选相对路径，顺序与客户端 SDK 的查找顺序一致（`V()`）。
const CANDIDATES: &[&str] = &[
    "dist/_worker/qoder-worker-runtime.obf.mjs",
    "dist/_worker/qoder-worker-runtime.mjs",
    "_worker/qoder-worker-runtime.obf.mjs",
    "_worker/qoder-worker-runtime.mjs",
];

/// 注入段里记下的元信息（从文件头解析，不执行任何 JS）
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    /// 注入的端点
    pub url: String,
    /// 注入的 CA 指纹（sha256 前 16 位十六进制），用于判断 CA 换过没有
    pub ca: String,
    /// 注入体指纹。是后加的字段：旧格式 marker 解析出来是 `None`，
    /// [`is_current`] 会因此判它过时、下一次心跳重写（升级自愈）。
    pub js: Option<String>,
}

/// 解析 worker 产物的**全部候选路径**：每个 SDK 根 × [`CANDIDATES`]，
/// 只收**真实存在**的文件，按探测顺序去重。
///
/// 为什么是复数：客户端 fast-update 之后，安装根里可能同时存在
/// 顶层原件与 `.qoder-versions/<版本>/` 里的多份副本，而「应用此刻执行哪一份」
/// 只有它自己知道（本机实测 0.4.1 已 stage 而应用仍跑 0.3.4）。
/// 接管因此**全量注入**：漏掉任何一份都可能「补丁打在没人读的文件上」
/// —— 2026-09-24 真机踩过：顶层与国际版版本目录并存，应用读的是版本目录那份。
pub fn worker_paths(region: Region) -> Vec<PathBuf> {
    let roots = match sdk_root_override() {
        Some(base) => vec![base.join(region.key())],
        None => region.worker_sdk_roots(),
    };
    let mut out = Vec::new();
    for root in &roots {
        for c in CANDIDATES {
            let p = root.join(c);
            if p.is_file() && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// 主落点：[`worker_paths`] 的第一个（版本目录优先于顶层，见
/// [`crate::region::Region::worker_sdk_roots`]）。报错文案与还原提示用它 ——
/// 真正的装卸走 [`install_everywhere`] / [`uninstall_everywhere`]，不认这个。
pub fn worker_path(region: Region) -> Option<PathBuf> {
    worker_paths(region).into_iter().next()
}

// ---------------------------------------------------------------------------
// 测试专用的 SDK 根替换
// ---------------------------------------------------------------------------

// 把 SDK 根指向临时目录。没有它，装卸流程的测试就只能对着 `/Applications` 里
// 真实的客户端跑 —— 那会把用户装好的客户端改坏，所以生产路径**永远**走
// `Region::worker_sdk_roots()`，这里只在 `#[cfg(test)]` 下被写入。
thread_local! {
    /// 测试里的 SDK 根覆盖，**必须是线程局部**。
    ///
    /// 用一个进程级 `Mutex` 会让并行跑的用例互相串台：A 用例刚把根指向自己的临时目录，
    /// B 用例就覆盖成了它的 —— 表现是「注入写进了别人的沙箱」这种查不出原因的随机失败。
    /// 生产路径从不写这个格子（真值永远来自 [`Region::worker_sdk_roots`]），所以线程局部
    /// 不会带来行为差异。
    static SDK_ROOT_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn sdk_root_override() -> Option<PathBuf> {
    SDK_ROOT_OVERRIDE.with(|c| c.borrow().clone())
}

/// 在临时 SDK 根下跑一段流程，结束后自动复位（panic 也复位）。
///
/// 两个区域会落在 `<base>/cn` 与 `<base>/global` 两个子目录里，
/// 这样「换区域要先卸旧的」这类不变量才测得出来。
#[cfg(test)]
pub fn with_sdk_root<T>(base: &Path, f: impl FnOnce() -> T) -> T {
    let previous = SDK_ROOT_OVERRIDE.with(|c| c.replace(Some(base.to_path_buf())));
    struct Reset(Option<PathBuf>);
    impl Drop for Reset {
        fn drop(&mut self) {
            SDK_ROOT_OVERRIDE.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let _reset = Reset(previous);
    f()
}

/// 按客户端内的目录布局，在给定根下铺一份可写的 worker 产物。
#[cfg(test)]
pub fn plant_worker(base: &Path, region: Region, body: &str) -> PathBuf {
    let dir = base.join(region.key()).join("dist").join("_worker");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("qoder-worker-runtime.obf.mjs");
    fs::write(&p, body).unwrap();
    p
}

fn backup_path(worker: &Path) -> PathBuf {
    let name = worker
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "worker".into());
    worker.with_file_name(format!("{name}.qoderassistant-orig"))
}

fn fingerprint16(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex(&h.finalize())[..16].to_string()
}

fn ca_fingerprint(ca_pem: &str) -> String {
    fingerprint16(ca_pem)
}

/// 注入体（标记行之后那段 JS）的指纹。
///
/// 它是「装着的是不是**这一版**注入代码」的判据：指到它，改 `render_js` 的产出
/// 就会让旧注入立刻变成「过时」，心跳重写 —— 不能只比 url/CA，那两个在改代码时
/// 不会变（2026-09-29 实遇：dev 重编译后新判据写不进产物）。
fn body_fingerprint(js: &str) -> String {
    fingerprint16(js)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 读文件头并解析注入标记；没注入过返回 None。
pub fn read_marker(worker: &Path) -> Option<Marker> {
    let mut f = fs::File::open(worker).ok()?;
    let mut buf = vec![0u8; HEAD_BYTES];
    let n = f.read(&mut buf).ok()?;
    parse_marker(&String::from_utf8_lossy(&buf[..n]))
}

/// 纯函数版：从（文件头的）文本里解析标记，便于单测。
pub fn parse_marker(head: &str) -> Option<Marker> {
    if !head.starts_with(MARK_BEGIN) {
        return None;
    }
    let line = head.lines().next()?;
    let url = field(line, "url=")?;
    let ca = field(line, "ca=")?;
    Some(Marker {
        url,
        ca,
        js: field(line, "js="),
    })
}

/// 从标记行里取 `key=value`（值到空格或行尾为止）
fn field(line: &str, key: &str) -> Option<String> {
    let start = line.find(key)? + key.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let v = &rest[..end];
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// 已注入（且端点 / CA / **注入体**都与当前构建一致）时返回 true。
///
/// 注入体也要比：只比 url/CA 的话，改了注入段代码的构建会把「装着旧注入」误判成
/// 最新 —— 心跳永远跳过重写，界面上全绿、客户端跑的还是旧代码。旧格式 marker
/// 没有 `js=` 字段 → 判不一致 → 下一次心跳升级重写。
pub fn is_current(worker: &Path, region: Region, url: &str, ca_pem: &str) -> bool {
    let Some(m) = read_marker(worker) else {
        return false;
    };
    m.url == url
        && m.ca == ca_fingerprint(ca_pem)
        && matches!(
            (m.js.as_deref(), render_js(region, url, ca_pem)),
            (Some(js), Ok(expected)) if js == body_fingerprint(&expected)
        )
}

/// 剥掉注入段，得到官方原文。没注入过就原样返回。
fn strip(raw: &str) -> &str {
    if !raw.starts_with(MARK_BEGIN) {
        return raw;
    }
    match raw.find(MARK_END) {
        Some(i) => {
            let rest = &raw[i + MARK_END.len()..];
            rest.trim_start_matches('\n').trim_start_matches('\r')
        }
        // 开标记在、结尾标记没了 —— 文件被截断或被人手改过。
        // 此时**不动它**：宁可接管不生效，也不能把一个坏文件写回去。
        None => raw,
    }
}

/// 从 `https://host[:port]` 里取端口。
///
/// 国际版的重定向要往 `tls.connect` 的 `port` 选项里填**数字**（那里不认 URL），
/// 所以这里把端点 URL 拆一次；缺端口按 https 默认 443。
fn port_of(url: &str) -> Result<u16, String> {
    let hostport = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    match hostport.rsplit_once(':') {
        Some((_, p)) => p
            .parse::<u16>()
            .map_err(|_| format!("端点 URL 里的端口不是数字：{url}")),
        None => Ok(443),
    }
}

/// 生成注入段 = 标记行 + 注入体（[`render_js`]）。
///
/// 标记行里的 `js=` 是注入体指纹，算的是**除标记行之外**的部分 —— 指纹不能写进
/// 自己覆盖的范围里（自指），所以先渲染本体、再拼标记行。
fn render(region: Region, url: &str, ca_pem: &str) -> Result<String, String> {
    let js = render_js(region, url, ca_pem)?;
    Ok(format!(
        "{} url={} ca={} js={} */\n{js}",
        MARK_BEGIN,
        url,
        ca_fingerprint(ca_pem),
        body_fingerprint(&js)
    ))
}

/// 注入体本身（标记行之后那段 JS）。
///
/// 所有插进 JS 的字符串都用 `{:?}`（Rust 字符串字面量）渲染，避免手写转义 ——
/// 这段代码一旦语法出错，客户端会**整个起不来**，不是「接管不生效」那么轻。
fn render_js(region: Region, url: &str, ca_pem: &str) -> Result<String, String> {
    let ca_lines = ca_pem
        .lines()
        .map(|l| format!("{l:?}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut s = String::with_capacity(ca_pem.len() + 3072);
    s.push_str("import{createRequire as __qaReq}from\"node:module\";\n");
    // 区域差异只有一处：怎么把流量引到本机。CA 注入两条路共用。
    let redirect_port = match region.takeover() {
        Takeover::EndpointEnv(key) => {
            s.push_str(&format!("process.env[{key:?}]={url:?};\n"));
            None
        }
        Takeover::HostRedirect => Some(port_of(url)?),
    };
    s.push_str("try{const __qaTls=__qaReq(import.meta.url)(\"node:tls\");");
    s.push_str(&format!("const __qaCa=[{ca_lines}].join(\"\\n\");"));
    s.push_str("const __qaLocal=h=>h===\"127.0.0.1\"||h===\"localhost\"||h===\"::1\";");
    s.push_str("const __qaHost=o=>String((o&&(o.host??o.hostname??o.servername))??\"\");");
    if let Some(port) = redirect_port {
        // 族规则取自 [`crate::region::MODEL_GATEWAY_HOST_REGEX`]（唯一来源）：
        // 反代认哪个族，客户端就只改哪个族的目标
        s.push_str(&format!(
            "const __qaGw=new RegExp({:?});",
            crate::region::MODEL_GATEWAY_HOST_REGEX
        ));
        s.push_str("const __qaSni=o=>String((o&&o.servername)??\"\");");
        s.push_str(&format!("const __qaPort={port};"));
    }
    s.push_str("const __qaConnect=__qaTls.connect;");
    s.push_str("__qaTls.connect=function(...a){try{const o=a[0];if(o&&typeof o===\"object\"){");
    if redirect_port.is_some() {
        // 只动**族内**的连接：族外的域（center / openapi）一个字段都不碰 ——
        // 连 servername 也不设，Node 本来就会拿 host 当 SNI，替它设一遍只是多一个
        // 「给 IP 塞 servername」的机会（Node 对 IP 的 SNI 另有一套判断）。
        // 保留 servername 的理由见 [`crate::region::Takeover::HostRedirect`]：
        // 选举会把推理域在 api1/api2/api3/api6… 之间换，写死一个上游就会「明天 404」。
        // `host` 与 `hostname` 都改：Node 的不同连接路径读的是其中一个，
        // 而两个都写只影响「连哪儿」，不影响 SNI。`socket` 在时跳过（那时 host/port 不生效）。
        // **族判据要连 servername 一起测**：httpdns 的 resolved-ip 路线里 host 是
        // 解析出的 IP，族名只留在 servername（见模块头注释）；只测 host 会整类漏掉。
        // `__qaGw.test(h)` 里的 `.test(` 不能省成 `__qaGw(h)`：`new RegExp` 出来的是
        // 正则对象，**不是函数**，当函数调用会在运行时抛 `TypeError` —— 而它会被包裹层
        // 自己的 `catch{}` 吞掉，表现是「注入装好了、界面显示生效中，实际一个连接都没改」。
        s.push_str(
            "if(!o.socket){const __qaH=__qaHost(o);if(__qaGw.test(__qaH)||__qaGw.test(__qaSni(o))){\
             if(o.servername===undefined)o.servername=__qaH;\
             o.host=\"127.0.0.1\";o.hostname=\"127.0.0.1\";o.port=__qaPort}}",
        );
    }
    s.push_str(
        "if(__qaLocal(__qaHost(o))){if(o.ca===undefined)o.ca=__qaCa;\
         o.rejectUnauthorized=!1}}}catch{}return __qaConnect.apply(this,a)};",
    );
    s.push_str("const __qaCsc=__qaTls.createSecureContext;");
    s.push_str(
        "__qaTls.createSecureContext=function(o){try{if(o&&typeof o===\"object\"\
         &&__qaLocal(__qaHost(o)))o=Object.assign({},o,\
         {ca:o.ca===undefined?__qaCa:o.ca,rejectUnauthorized:!1})}catch{}\
         return __qaCsc.call(this,o)}}catch{}\n",
    );
    s.push_str(MARK_END);
    s.push('\n');
    Ok(s)
}

/// 从当前可执行文件回溯出它所在的 `.app` 包。
///
/// 「该授权给谁」完全取决于本进程的形态：应用包形态能回溯出 `.app`，
/// 而开发模式的裸二进制（`target/debug/…`）没有包 —— 那时「App 管理」列表里
/// 根本没有对应项，用户在系统设置里开的授权也落不到它身上。
fn app_bundle_of(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
}

/// 「去系统设置里给谁开权限」—— 整套话术只有这一处出口，免得界面与日志各说一套。
///
/// 成因只有一个：macOS 的「App 管理」(TCC) 会拦住对**其他已签名应用包**的修改，
/// 按请求进程的代码身份判定 —— **绕开 sandbox、加 sudo 都没用**。
///
/// ⚠️ 实测（tccd 日志，2026-09-19）：这个服务**不会为自签名的二进制弹授权框**，
/// 它直接在 tccd 里落一条 deny 就完事：
///
/// ```text
/// Failed to match existing code requirement for subject com.waxilo.qoder-assistant
///   and service kTCCServiceSystemPolicyAppBundles
/// Service kTCCServiceSystemPolicyAppBundles does not allow prompting for unentitled
///   binaries; returning denied.
/// ```
///
/// 所以话术里**不能**写「请在弹窗里点允许」—— 那个框永远不会出现，用户会一直干等。
/// 唯一的路是**手动**去「App 管理」把开关打开（请求过一次之后，系统已经替该应用建好
/// 条目，只是默认关着）。
///
/// 授权能否"记得住"取决于**签名身份**：本应用用固定自签证书签名（`QoderAssistant
/// Self-Signed`，见 `scripts/make-signing-cert.sh`），DR 锚在叶证书哈希上。
///
/// ⚠️ 但「锚在证书上」只保证**新申请的授权**跨构建有效。tccd 里那条记录存的是
/// **申请那一刻**的 requirement —— 如果那条是在还挂着 ad-hoc 签名（DR 只有 cdhash）
/// 的时候建起来的，之后换成证书签名就再也匹配不上，实测日志：
/// `Failed to match existing code requirement for subject … and service
/// kTCCServiceSystemPolicyAppBundles`。此时开关怎么拨都没用，必须**先删掉旧条目再重新
/// 添加**，让系统按当前的 DR 重建记录。所以话术里两条都要写，别只写「把开关打开」。
fn authorize_hint() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    authorize_hint_for(app_bundle_of(&exe).as_deref(), &exe)
}

/// 话术本体。抽成纯函数是为了能被测 ——「该授权给谁」取决于运行形态，
/// 而测试进程自己就是个裸二进制，没法靠真跑一遍来验证另一条分支。
fn authorize_hint_for(app: Option<&Path>, exe: &Path) -> String {
    match app {
        Some(app) => {
            let name = app
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "本应用".to_string());
            format!(
                "去「系统设置 → 隐私与安全性 → App 管理」把「{name}」的开关打开（{}）。\
                 这个服务**不会弹授权框**（系统只会在 tccd 里记一条拒绝），必须手动开、别等弹窗；\
                 列表里若没有本应用，点左下角「+」从 /Applications 添加。\
                 如果条目**已经在**、开关也开着却仍然被拒，那是 tccd 里存的旧授权要求跟当前\
                 构建不匹配（重新签名/换过签名方式之后会这样）—— 要先点「−」删掉旧条目，\
                 再「+」重新添加。任一种改动之后都要**重启本应用**才生效。",
                app.display()
            )
        }
        None => format!(
            "⚠️ 本次运行的是**开发模式**的裸二进制（{}）：它没有应用包，\
             「App 管理」列表里没有对应项 —— 在那里开的授权落不到它身上。\
             请改用应用包形态运行。",
            exe.display()
        ),
    }
}

/// 写产物（以及它的备份）—— 撞 `EPERM` 时**换 inode 重写**。
///
/// 直接 `fs::write` 是「原地改这个 inode」，而 macOS 15+ 在文件上记了一份
/// `com.apple.provenance`：**这个文件归哪个代码身份写**。本应用重新构建 / 自更新之后
/// cdhash 变了，于是「上一版构建注入的产物，这一版改不动」—— 表现就是那句
/// `Operation not permitted`，而「App 管理」里开关明明是开着的，白查半天。
///
/// 换 inode 绕开的就是这一层：先写同目录的临时文件（新建的文件由**当前**身份取得归属），
/// 再 `rename` 顶掉目标。顺序不能反 —— 先删后写会在删除成功、写入失败时把客户端的
/// 产物整个丢掉；`rename` 是原子的，最坏情况只是留下一个没人读的临时文件。
///
/// 权限位要一起搬过去：那份产物是 `0755`，用默认 umask 新建会变成 `0644`。
fn write_artifact(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match fs::write(path, bytes) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            write_via_temp(path, bytes).map_err(|_| e)
        }
        Err(e) => Err(e),
    }
}

fn write_via_temp(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mode = file_mode(path);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "worker".into());
    let tmp = path.with_file_name(format!("{name}.qoderassistant-new"));
    fs::write(&tmp, bytes)?;
    if let Some(mode) = mode {
        set_file_mode(&tmp, mode)?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// 权限位只在 unix 上有意义（Windows 的 ACL 不归我们管）。
fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        fs::metadata(path).map(|m| m.permissions().mode()).ok()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn set_file_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// 把「写不进去」翻译成一句**用户能照做**的话。
///
/// 走到这里说明连换 inode 的重写也被拒了。剩下的来源是 macOS 13+ 的「App 管理」
/// （App Management）—— 一个进程想改**另一个已签名应用包**的内容，必须先获得用户授权。
/// 系统既不提示、也不给 403，只丢一句 `Operation not permitted`，
/// 很容易被当成「路径写错 / 文件只读」而白查半天。
///
/// 这条消息会一路透到界面的接管状态里，所以它必须自带下一步动作，而不是只报错误码。
fn write_hint(what: &str, path: &Path, e: &std::io::Error) -> String {
    let base = format!("{what}失败（{}）：{e}", path.display());
    if e.kind() != std::io::ErrorKind::PermissionDenied {
        return base;
    }
    format!("{}。{}", base, authorize_hint())
}

/// 把注入段写进 worker 产物。幂等：已是最新就什么都不做。
///
/// 返回「是否真的写过盘」（`false` = 已经是最新的，跳过）。
pub fn install_at(worker: &Path, region: Region, url: &str, ca_pem: &str) -> Result<bool, String> {
    if is_current(worker, region, url, ca_pem) {
        return Ok(false);
    }
    let raw = fs::read_to_string(worker)
        .map_err(|e| write_hint("读取 worker 产物", worker, &e))?;
    let had_marker = raw.starts_with(MARK_BEGIN);

    // 备份只在「当前文件就是官方原版」时刷新 —— 这样备份永远是好的那份，
    // 不会出现「备份是上一个版本、还原把用户的新版客户端降级」。
    //
    // 条件必须是「文件里没有我们的注入段」，**不能**再叠一个 `!backup.exists()`：
    // 那样官方更新一次之后就再也不刷新了，备份会永远停在**上一版**官方文件 ——
    // 恰好就是这段注释要防的那件事。
    if !had_marker {
        let backup = backup_path(worker);
        write_artifact(&backup, raw.as_bytes()).map_err(|e| write_hint("备份原文件", &backup, &e))?;
    }

    let body = render(region, url, ca_pem)?;
    let stripped = strip(&raw);
    write_artifact(worker, format!("{body}{stripped}").as_bytes())
        .map_err(|e| write_hint("写入 worker 产物", worker, &e))?;
    Ok(true)
}

/// 精确剥离注入段。返回是否真的改过文件。
pub fn uninstall_at(worker: &Path) -> Result<bool, String> {
    let raw = fs::read_to_string(worker)
        .map_err(|e| format!("读取 worker 产物失败（{}）：{e}", worker.display()))?;
    if !raw.starts_with(MARK_BEGIN) {
        return Ok(false);
    }
    let stripped = strip(&raw);
    if stripped.len() == raw.len() {
        // 只有开标记、没有结尾标记：不是我们完整写的，拒绝改动
        return Err(format!(
            "{} 里的接管注入段不完整，已跳过（手动删除该段或重装客户端）",
            worker.display()
        ));
    }
    write_artifact(worker, stripped.as_bytes())
        .map_err(|e| write_hint("还原 worker 产物", worker, &e))?;
    Ok(true)
}

/// 本应用写不回去时，给用户一条能直接粘进「终端」的还原命令。
///
/// 关闭接管这条路上，还原失败会把整次保存一起拒掉（故意的 —— 留下「客户端指着一个
/// 已经不监听的端口」比开关拨不动恶劣得多）。代价是用户可能一直卡在「关不掉」，
/// 所以报错必须自带出口，而不是只说「去开权限然后重来」。
///
/// 备份可以直接用：`install_at` 只在**文件没有我们的标记**时刷新备份，所以只要标记还在，
/// 那份备份就是当前这份产物的官方原文（逐字节比对已验证）。备份不在时宁可不给命令，
/// 也不能让人拿一份旧版客户端去覆盖新版。
pub fn restore_hint(region: Region) -> Option<String> {
    let worker = worker_path(region)?;
    let backup = backup_path(&worker);
    if !backup.is_file() {
        return None;
    }
    Some(format!(
        "或者直接在「终端」里执行这条命令还原（等价于本应用的摘除）：cp {} {}",
        shell_quote(&backup),
        shell_quote(&worker)
    ))
}

/// 单引号包住，里面的单引号按 POSIX 的 `'\''` 写法转义。
/// 客户端路径里带空格（`/Applications/Qoder CN.app/…`），不引起来那条命令是错的。
fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("'{}'", s.replace('\'', r"'\''"))
}

// 这之上曾有一对 `patch::install(region, url, ca)` / `patch::uninstall(region)` 封装。
// 它们被删掉了：真正需要它们的 `stealth` 还得拿到产物路径去写接管日志、并且要给出
// 更贴用户场景的报错（「客户端没装在 /Applications」），于是自己解析路径、直接调
// `install_at` / `uninstall_at`。留着这两个封装只会变成一份没人走、却会被误当成
// 正门的分叉。

/// 把注入写进**每一份**真实存在的产物副本（见 [`worker_paths`]）。
///
/// 返回 `(已确认注入的路径（新写或原本就一致）, 失败的 (路径, 原因))`。
/// 一份都不存在 → `Err`（客户端没装的形态，调用方照 [`crate::region::Region::install_hint`]
/// 报错）；**部分失败不算整体失败** —— 真正在跑的那份可能已经在成功的那部分里，
/// 把整体推翻回滚反而会让接管「看着开着、其实被摘了」。失败清单由调用方记进调试日志。
pub fn install_everywhere(
    region: Region,
    url: &str,
    ca_pem: &str,
) -> Result<(Vec<PathBuf>, Vec<(PathBuf, String)>), String> {
    let paths = worker_paths(region);
    if paths.is_empty() {
        return Err(format!(
            "找不到{}客户端的 worker 产物：请确认官方客户端已安装在 {}",
            region.label(),
            region.install_hint()
        ));
    }
    let mut installed = Vec::new();
    let mut failed = Vec::new();
    for p in paths {
        match install_at(&p, region, url, ca_pem) {
            Ok(_) => installed.push(p),
            Err(e) => failed.push((p, e)),
        }
    }
    if installed.is_empty() {
        // 一份都没写上：把第一个原因端出去（其余在 failed 里，调用方可留痕）
        let first = failed.remove(0);
        return Err(first.1);
    }
    Ok((installed, failed))
}

/// 从**每一份**真实存在的产物里剥离注入（见 [`worker_paths`]）。
///
/// 返回确实改了盘的份数。**失败不打断**：一份剥离失败（注入段不完整 / 被占用）时
/// 继续剥其余的，最后把第一个原因报出去 —— 关接管的路上「留下一份还指着死端口的
/// 副本」比「报错」恶劣，能救几份是几份。没有副本时不报错（客户端已卸载的形态）。
pub fn uninstall_everywhere(region: Region) -> Result<usize, String> {
    let paths = worker_paths(region);
    let mut changed = 0;
    let mut first_err: Option<String> = None;
    for p in paths {
        match uninstall_at(&p) {
            Ok(true) => changed += 1,
            Ok(false) => {}
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(changed),
    }
}

/// 该区域当前是否已注入（不看内容是否最新，只看有没有）——
/// **任何一份**副本上还在，就算还在（`uninstall_everywhere` 会把每一份都剥掉）。
pub fn is_installed(region: Region) -> bool {
    worker_paths(region).iter().any(|p| read_marker(p).is_some())
}

/// 该区域当前注入的端点：第一份带标记的副本上写的那个。
pub fn current_url(region: Region) -> Option<String> {
    worker_paths(region)
        .iter()
        .find_map(|p| read_marker(p).map(|m| m.url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    const FAKE_CA: &str = "-----BEGIN CERTIFICATE-----\nMIIBfakeAAAA\nMIIBfakeBBBB\n-----END CERTIFICATE-----\n";
    const ORIGINAL: &str = "const _$d=(s,k)=>s;\nimport{createRequire as __banner_createRequire}from\"node:module\";\n";

    fn tmp_worker() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qa-patch-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("qoder-worker-runtime.obf.mjs");
        fs::write(&p, ORIGINAL).unwrap();
        p
    }

    /// 「该授权给谁」完全取决于本进程的形态：应用包能回溯出 `.app`，裸二进制不能。
    #[test]
    fn app_bundle_is_found_only_for_a_bundled_binary() {
        assert_eq!(
            app_bundle_of(Path::new("/Applications/Foo.app/Contents/MacOS/foo")).as_deref(),
            Some(Path::new("/Applications/Foo.app")),
            "应用包形态应当能回溯出 .app 目录"
        );
        assert!(
            app_bundle_of(Path::new("/x/src-tauri/target/debug/qoder-assistant")).is_none(),
            "开发模式的裸二进制没有应用包"
        );
        assert!(app_bundle_of(Path::new("")).is_none(), "空路径不应 panic");
    }

    /// 话术必须点名「被拦的是哪个文件」——否则用户拿着它判断不出自己开对了开关没有。
    #[test]
    fn permission_denied_hint_names_the_blocked_path() {
        let e = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let msg = write_hint(
            "写入 worker 产物",
            Path::new("/Applications/X.app/Resources/w.mjs"),
            &e,
        );
        assert!(msg.contains("/Applications/X.app/Resources/w.mjs"));
        // 「App 管理」两条分支里都点名了，所以这条在测试进程（裸二进制）下也成立
        assert!(msg.contains("App 管理"), "要指出该去开哪个开关：{msg}");
    }

    /// 应用包形态下的完整话术：点名要授权谁，并且说清两条**实测**出来的前提 ——
    /// **不会弹窗**（必须手动开）、**重启才生效**。
    /// 写成「请在弹窗里允许」会让用户一直等一个永远不会出现的框（tccd 日志原话：
    /// `does not allow prompting for unentitled binaries`）。
    #[test]
    fn bundled_authorization_hint_says_manual_and_restart() {
        let msg = authorize_hint_for(
            Some(Path::new("/Applications/QoderAssistant.app")),
            Path::new("/Applications/QoderAssistant.app/Contents/MacOS/qoder-assistant"),
        );
        assert!(msg.contains("QoderAssistant"), "要点名该允许谁：{msg}");
        assert!(msg.contains("/Applications/QoderAssistant.app"), "{msg}");
        assert!(msg.contains("不会弹"), "必须说清不会弹授权框：{msg}");
        assert!(msg.contains("手动"), "要说清得手动开：{msg}");
        assert!(msg.contains("重启"), "授权后要重启才生效：{msg}");
    }

    /// 裸二进制（开发模式）要如实说「这个开关对你没用」，而不是给一句照做也没用的指引。
    #[test]
    fn bare_binary_hint_points_at_the_missing_bundle() {
        let msg = authorize_hint_for(None, Path::new("/x/target/debug/qoder-assistant"));
        assert!(msg.contains("裸二进制"), "{msg}");
        assert!(msg.contains("/x/target/debug/qoder-assistant"), "{msg}");
        assert!(!msg.contains("系统设置"), "别把它引去一个对它无效的开关：{msg}");
    }

    /// 别的 IO 错误**不能**被硬套成「权限问题」—— 那会把人引到系统设置里白翻一遍。
    #[test]
    fn other_io_errors_are_not_dressed_up_as_a_permission_problem() {
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        let msg = write_hint("写入 worker 产物", Path::new("/nope/w.mjs"), &e);
        assert!(msg.contains("/nope/w.mjs"));
        assert!(!msg.contains("App 管理"), "别把找不到文件说成权限问题：{msg}");
    }

    /// 「本应用改不动这份产物」时得换 inode 重写。
    ///
    /// 这里用只读位当替身：真实场景是 macOS 的 `com.apple.provenance` 按 cdhash 记
    /// 「这文件归谁写」，本应用重新构建之后就成了「上一版注入的产物，这一版写不回去」
    /// —— 同样是 `fs::write` 被拒、而目录本身可写。两者的区别在界面上看不出来，
    /// 所以兜底必须对这一整类成立，而不是只对某一种成因。
    #[cfg(unix)]
    #[test]
    fn an_unwritable_artifact_is_rewritten_through_a_new_inode() {
        use std::os::unix::fs::PermissionsExt;
        let p = tmp_worker();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o555)).unwrap();

        assert!(install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap());
        assert!(fs::read_to_string(&p).unwrap().starts_with(MARK_BEGIN));
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o555,
            "新建的文件要搬回原来的权限位：产物是 0755，默认 umask 会写成 0644"
        );

        assert!(uninstall_at(&p).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), ORIGINAL, "还原必须逐字节一致");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    /// 兜底也写不进去时：报**原来那个**错误，并且不许碰过目标文件。
    #[cfg(unix)]
    #[test]
    fn a_failed_fallback_reports_the_original_error_and_keeps_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let p = tmp_worker();
        install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap();
        let before = fs::read_to_string(&p).unwrap();
        let dir = p.parent().unwrap();
        // 文件写不动（触发兜底），目录也写不动（兜底建不出临时文件）
        fs::set_permissions(&p, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
        let e = write_artifact(&p, b"x").unwrap_err();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");
        assert_eq!(fs::read_to_string(&p).unwrap(), before);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn restore_command_quotes_paths_that_contain_spaces() {
        assert_eq!(
            shell_quote(Path::new("/Applications/Qoder CN.app/w.mjs")),
            "'/Applications/Qoder CN.app/w.mjs'"
        );
        assert_eq!(shell_quote(Path::new("/a/it's.mjs")), r"'/a/it'\''s.mjs'");
    }

    /// 没有备份就宁可不给命令：拿一份旧版客户端的原版去覆盖新版，比不给命令更糟。
    #[test]
    fn restore_hint_appears_only_once_a_backup_exists() {
        let base = std::env::temp_dir().join(format!("qa-restore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        with_sdk_root(&base, || {
            let p = plant_worker(&base, Region::Cn, ORIGINAL);
            assert!(restore_hint(Region::Cn).is_none(), "还没注入过，没有可还原的东西");
            install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap();
            let hint = restore_hint(Region::Cn).unwrap();
            assert!(hint.contains("cp "), "{hint}");
            assert!(hint.contains(".qoderassistant-orig' '"), "两个路径都要引起来：{hint}");
        });
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn install_then_uninstall_round_trips_byte_for_byte() {
        let p = tmp_worker();
        assert!(install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap());
        let injected = fs::read_to_string(&p).unwrap();
        assert!(injected.starts_with(MARK_BEGIN));
        assert!(injected.ends_with(ORIGINAL), "原文必须原样接在注入段之后");
        assert!(injected.contains("QODERCN_SERVER_ENDPOINT"));

        assert!(uninstall_at(&p).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), ORIGINAL, "还原必须逐字节一致");
        // 二次摘除是空操作，不是错误
        assert!(!uninstall_at(&p).unwrap());
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn install_is_idempotent_and_refreshes_on_url_change() {
        let p = tmp_worker();
        assert!(install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap());
        // 同样的参数：不该再写盘（心跳每 20s 一次，不能每次重写 33MB）
        assert!(!install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap());
        // 换了端口：重写，且原文只有一份（不会叠加两段注入）
        assert!(install_at(&p, Region::Cn, "https://127.0.0.1:9999", FAKE_CA).unwrap());
        let s = fs::read_to_string(&p).unwrap();
        assert_eq!(s.matches(MARK_BEGIN).count(), 1, "注入段不能叠加：{s:.200}");
        assert_eq!(s.matches("https://127.0.0.1:9999").count() > 0, true);
        // 旧端点不能残留。判据写成完整 URL 而不是裸 `"8789"`：标记行里的
        // js 指纹是十六进制，有极小概率自然含 "8789" —— 裸串断言会偶发假失败。
        assert!(!s.contains("127.0.0.1:8789"));
        assert_eq!(read_marker(&p).unwrap().url, "https://127.0.0.1:9999");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn marker_records_the_ca_so_a_rotated_ca_triggers_a_rewrite() {
        let p = tmp_worker();
        install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap();
        assert!(is_current(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA));
        let other_ca = FAKE_CA.replace("AAAA", "ZZZZ");
        assert!(!is_current(&p, Region::Cn, "https://127.0.0.1:8789", &other_ca));
        assert!(install_at(&p, Region::Cn, "https://127.0.0.1:8789", &other_ca).unwrap());
        assert_eq!(read_marker(&p).unwrap().ca, ca_fingerprint(&other_ca));
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    /// 官方更新之后：文件变回原版，注入要「自愈」回去，并且备份要刷新成新版
    #[test]
    fn re_installs_after_the_client_updates_over_us() {
        let p = tmp_worker();
        install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap();
        let backup = backup_path(&p);
        assert!(backup.exists());

        // 模拟官方更新：整份文件被换成新的原版
        let updated = format!("{ORIGINAL}// v2\n");
        fs::write(&p, &updated).unwrap();
        assert!(read_marker(&p).is_none());

        assert!(install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap());
        assert_eq!(
            fs::read_to_string(&backup).unwrap(),
            updated,
            "备份要刷新成官方新版，而不是留着上一版"
        );
        let injected = fs::read_to_string(&p).unwrap();
        assert!(injected.ends_with(&updated));
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    /// 只有开标记、没有结尾标记（被人手改坏）：拒绝写入，别把坏文件落盘
    #[test]
    fn refuses_to_touch_a_truncated_injection() {
        let p = tmp_worker();
        fs::write(&p, format!("{MARK_BEGIN} url=x ca=y */\nconst a=1;\n")).unwrap();
        assert!(uninstall_at(&p).is_err());
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn marker_parsing_reads_what_render_wrote() {
        let body = render(Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap();
        let m = parse_marker(&body).expect("应能解析");
        assert_eq!(m.url, "https://127.0.0.1:8789");
        assert_eq!(m.ca, ca_fingerprint(FAKE_CA));
        // 注入体指纹必须是**本体**的哈希（标记行之外），且渲染是确定性的
        let js = render_js(Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap();
        let fp = body_fingerprint(&js);
        assert_eq!(m.js.as_deref(), Some(fp.as_str()), "js= 指纹要对得上注入体");
        assert!(!js.contains(MARK_BEGIN), "指纹不能含标记行（自指）：{js:.80}");
        // 未经注入的文件头解析不出来
        assert!(parse_marker(ORIGINAL).is_none());
    }

    /// 改了注入段代码（url/CA 都没变）也要触发重写 —— marker 里的 `js=` 指纹
    /// 与当前构建对不上就该重写。2026-09-29 真机：dev 重编译出带新判据的构建、
    /// 产物里却还是旧注入，就是缺这个判据（快速重建时租约没过期，心跳直接跳过）。
    #[test]
    fn a_changed_injection_body_forces_a_rewrite() {
        let p = tmp_worker();
        install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap();
        assert!(is_current(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA));

        // 模拟「上一版构建注入的产物」：把标记行里的 js= 指纹改成过时的
        let s = fs::read_to_string(&p).unwrap();
        let (line, rest) = s.split_once('\n').unwrap();
        let stale_line = match line.split_once(" js=") {
            Some((head, tail)) => format!("{head} js=0000000000000000{}", &tail[16..]),
            None => panic!("标记行里应当有 js=：{line}"),
        };
        fs::write(&p, format!("{stale_line}\n{rest}")).unwrap();

        assert!(!is_current(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA));
        assert!(
            install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap(),
            "指纹对不上必须重写"
        );
        assert!(is_current(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA));
        let after = fs::read_to_string(&p).unwrap();
        assert_eq!(after.matches(MARK_BEGIN).count(), 1, "重写不能叠出两段注入");
        assert!(after.ends_with(ORIGINAL), "重写后官方原文仍要原样接在注入段之后");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    /// 旧格式（没有 `js=` 的）marker：判为过时 → 下一次 install 升级重写。
    /// 这条是「改了注入代码的构建第一次上线」真正会走的升级路径。
    #[test]
    fn an_old_format_marker_without_js_is_healed_by_the_next_install() {
        let p = tmp_worker();
        let js = render_js(Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap();
        fs::write(
            &p,
            format!(
                "{MARK_BEGIN} url=https://127.0.0.1:8789 ca={} */\n{js}",
                ca_fingerprint(FAKE_CA)
            ),
        )
        .unwrap();
        assert!(read_marker(&p).unwrap().js.is_none(), "旧格式应解析出 None");
        assert!(!is_current(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA));

        assert!(install_at(&p, Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap());
        let fp = body_fingerprint(&js);
        assert_eq!(read_marker(&p).unwrap().js.as_deref(), Some(fp.as_str()));
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    /// 插入的 JS 必须是**语法合法**的：字符串字面量成对、没有裸的换行。
    /// （一段语法错误的注入会让客户端整个起不来，比接管不生效严重得多。）
    /// 两个区域的渲染结果都要过这一关 —— 国际版那段分支是后加的，更容易出错。
    #[test]
    fn rendered_js_is_syntactically_sane() {
        for region in Region::ALL {
            let body = render(region, "https://127.0.0.1:8789", FAKE_CA).unwrap();
            let tag = region.label();
            assert_eq!(
                body.matches('"').count() % 2,
                0,
                "[{tag}] 双引号必须成对：\n{body}"
            );
            assert!(body.contains("import{createRequire as __qaReq}from\"node:module\";"));
            assert!(body.contains("-----BEGIN CERTIFICATE-----"));
            assert!(body.contains("-----END CERTIFICATE-----"));
            assert!(body.trim_end().ends_with(MARK_END), "[{tag}]");
            // 每个字符串字面量里都不能出现裸换行
            for line in body.lines().filter(|l| l.contains("__qaCa=")) {
                assert!(!line.is_empty(), "[{tag}]");
            }
            // 端点 URL 只能以字符串字面量出现（裸着写进 JS，URL 里的 `//` 会变成注释）。
            // 标记行自己就写着 `url=…`，但它在 `/* */` 里，排除掉。
            for line in body.lines().filter(|l| !l.starts_with(MARK_BEGIN)) {
                assert!(
                    !line.contains("=https://"),
                    "[{tag}] 端点必须渲染成字符串字面量：{line}"
                );
            }
        }
    }

    /// 国际版的注入段：**改连接目标、保留 SNI**，且绝不写 env 键
    /// （国际版构建里那个键没人读，写了等于没写，界面却会显示「接管已开启」）。
    #[test]
    fn global_injection_redirects_the_gateway_family_and_keeps_sni() {
        let body = render(Region::Global, "https://127.0.0.1:8789", FAKE_CA).unwrap();
        // 端口必须是**数字**：tls.connect 的 port 选项不认 URL
        assert!(body.contains("const __qaPort=8789;"), "{body}");
        // 族规则与反代同源（同一个常量渲染出来），否则「注入改了、反代不认」
        assert!(
            body.contains(&format!(
                "new RegExp({:?})",
                crate::region::MODEL_GATEWAY_HOST_REGEX
            )),
            "族规则必须取自 region::MODEL_GATEWAY_HOST_REGEX：{body}"
        );
        // 只改 host/hostname/port，**不动 servername**（拿不到时才补上原 host）——
        // 反代按 SNI 选上游
        assert!(body.contains("if(o.servername===undefined)o.servername=__qaH"), "{body}");
        assert!(body.contains("o.host=\"127.0.0.1\""), "{body}");
        assert!(body.contains("o.hostname=\"127.0.0.1\""), "{body}");
        assert!(body.contains("o.port=__qaPort"), "{body}");
        // 族外（center / openapi）一个字段都不能碰：判据在改写**之前**取。
        // `.test(` 单独钉住：正则对象不可调用，`__qaGw(h)` 会抛 TypeError，
        // 而外层 catch 会把它吞成「静默不生效」。
        // servername 也必须进判据：httpdns 的 resolved-ip 路线只有它带着域名
        // （host 是解析出的 IP）—— 只测 host 会把整类对话连接漏掉。
        assert!(
            body.contains(
                "const __qaH=__qaHost(o);if(__qaGw.test(__qaH)||__qaGw.test(__qaSni(o))){"
            ),
            "族判据必须在改 host 之前算、走 RegExp.test，且连 servername 一起测：{body}"
        );
        assert!(
            body.contains("const __qaSni=o=>String((o&&o.servername)??\"\");"),
            "servername 的取值助手要能读到：{body}"
        );
        // 国际版没有可读的端点键：不许出现 env 赋值
        assert!(
            !body.contains("process.env["),
            "国际版不能退回 env 覆盖（那个键在国际版构建里恒不生效）：{body}"
        );
        // CA 注入两条路共用：重定向之后连接落在回环上，CA 才跟着注入
        assert!(body.contains("__qaLocal(__qaHost(o))"), "{body}");
        assert!(body.contains("-----BEGIN CERTIFICATE-----"), "{body}");
    }

    /// 国内版反过来：只写 env 键，**不碰** TLS 目标（它的端点覆盖本来就让客户端
    /// 自己连 127.0.0.1，去改目标只会多一份没用的注入面）。
    #[test]
    fn cn_injection_stays_env_only() {
        let body = render(Region::Cn, "https://127.0.0.1:8789", FAKE_CA).unwrap();
        assert!(
            body.contains("process.env[\"QODERCN_SERVER_ENDPOINT\"]=\"https://127.0.0.1:8789\";"),
            "{body}"
        );
        assert!(!body.contains("__qaGw"), "国内版不该带网关族规则：{body}");
        assert!(!body.contains("__qaSni"), "国内版不该带 SNI 判据：{body}");
        assert!(!body.contains("__qaPort"), "国内版不该带重定向端口：{body}");
    }

    /// 端点 URL 拆端口：缺端口按 https 默认 443，非数字要报错而不是静默取 0。
    #[test]
    fn redirect_port_is_parsed_from_the_endpoint_url() {
        assert_eq!(port_of("https://127.0.0.1:8789").unwrap(), 8789);
        assert_eq!(port_of("https://127.0.0.1").unwrap(), 443);
        assert_eq!(port_of("https://127.0.0.1:8789/").unwrap(), 8789);
        assert!(port_of("https://127.0.0.1:very").is_err());
    }

    /// 国际版也要能注入 / 摘除 / 还原 —— 走的是与国内版同一套落盘路径。
    #[test]
    fn global_inject_uninstall_round_trips_byte_for_byte() {
        let p = tmp_worker();
        assert!(install_at(&p, Region::Global, "https://127.0.0.1:8789", FAKE_CA).unwrap());
        let injected = fs::read_to_string(&p).unwrap();
        assert!(injected.starts_with(MARK_BEGIN));
        assert!(injected.ends_with(ORIGINAL), "原文必须原样接在注入段之后");
        assert_eq!(read_marker(&p).unwrap().url, "https://127.0.0.1:8789");

        assert!(uninstall_at(&p).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap(), ORIGINAL, "还原必须逐字节一致");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    // ── 行为验证：渲染结果交给 node **真跑一遍** ─────────────────────────
    //
    // 字符串断言测的是「我们写了什么」，测不出「跑起来会怎样」：注入段语法合法、
    // `node --check` 也过，仍可能一执行就抛。实测踩过这个坑 —— `__qaGw(h)` 把
    // 正则对象当函数调，抛出的 `TypeError` 被包裹层自己的 `catch{}` 吞掉，
    // 表现是「界面显示生效中、实际一个连接都没改」，纯静态检查一个都发现不了。
    //
    // 下面这套把注入段挂进一个真 node 进程，读回它**实际**改了哪些字段。
    // 需要本机的 node（不在 crate 依赖里）：缺席时打印一行跳过，不算失败。

    /// node 侧观测脚本：逐例调用 `tls.connect`，把**调用后**的选项对象打回来。
    ///
    /// `lookup` 被桩掉是有意的：族外的域（center / openapi）必须原样直连，
    /// 而测试不许碰网 —— 桩让「没被改写」这一例在尝试解析域名时就报错结束，
    /// 观测到的字段仍是改写与否的直接证据。族内的例子会被改写成本机回环端口
    /// （由 Rust 侧真实监听），连接落在我们的监听器上，同样不出网。
    ///
    /// `resolved-ip` 两例模拟 httpdns 路线的建连形态（host 已是解析出的 IP、
    /// 域名在 `servername`）：不涉及 DNS；未改写的反例用回环网段里的 `127.0.0.2`，
    /// 即使真的发起连接也是瞬时拒绝，同样不出网。
    const HARNESS: &str = r#"
import tls from "node:tls";
import { pathToFileURL } from "node:url";
await import(pathToFileURL(process.env.QA_INJECT).href);
const port = Number(process.env.QA_PORT);
const dns = (_h, _o, cb) => cb(new Error("qa: 测试不许碰网"));
const cases = [
  ["family-sh", { host: "api3.qoder.sh", port: 443 }],
  ["family-cn", { host: "api6.qoder.com.cn", port: 443 }],
  ["hostname-form", { hostname: "api2.qoder.sh", port: 443 }],
  ["resolved-ip", { host: "8.212.20.107", servername: "api3.qoder.sh", port: 443 }],
  ["resolved-ip-foreign-sni", { host: "127.0.0.2", servername: "example.com", port: 443 }],
  ["foreign-center", { host: "center.qoder.sh", port: 443 }],
  ["foreign-openapi", { host: "openapi.qoder.sh", port: 443 }],
  ["suffix-attack", { host: "api3.qoder.sh.evil.com", port: 443 }],
  ["double-tail", { host: "api3.qoder.sh.qoder.sh", port: 443 }],
  ["loopback", { host: "127.0.0.1", port }],
  ["localhost", { host: "localhost", port }],
];
for (const [label, opts] of cases) {
  const o = { ...opts, lookup: dns };
  const s = tls.connect(o);
  s.on("error", () => {});
  s.destroy();
  console.log(`QA ${label} host=${o.host} hostname=${o.hostname} port=${o.port} servername=${o.servername} ca=${o.ca ? "yes" : "no"} reject=${o.rejectUnauthorized}`);
}
if (process.env.QA_ENV_KEY) {
  console.log(`QA env-key value=${process.env[process.env.QA_ENV_KEY]}`);
}
"#;

    /// 从 node 的观测输出里取一行（按 `QA <例名>` 前缀），取不到直接把全部输出摊开。
    fn qa_line(out: &str, label: &str) -> String {
        let prefix = format!("QA {label} ");
        out.lines()
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| panic!("node 没给出「{label}」这一行：\n{out}"))
            .to_string()
    }

    /// 渲染一份注入 → 交给 node 执行 → 返回（回环监听端口, 观测输出）。
    /// node 缺席返回 None。
    fn observe_injection_with_node(region: Region) -> Option<(u16, String)> {
        if crate::proc::cmd("node")
            .arg("--version")
            .output()
            .is_err()
        {
            println!("本机没有 node，跳过注入段的真机行为验证");
            return None;
        }
        // 真监听一个回环端口：重定向之后的连接要有真实落点，观测才不是自说自话
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑回环端口");
        let port = listener.local_addr().unwrap().port();

        let dir = std::env::temp_dir().join(format!("qa-inject-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let inject = dir.join("inject.mjs");
        fs::write(
            &inject,
            render(region, &format!("https://127.0.0.1:{port}"), FAKE_CA).unwrap(),
        )
        .unwrap();
        let harness = dir.join("observe.mjs");
        fs::write(&harness, HARNESS).unwrap();

        let out = crate::proc::cmd("node")
            .arg(&harness)
            .env("QA_INJECT", &inject)
            .env("QA_PORT", port.to_string())
            .env(
                "QA_ENV_KEY",
                match region.takeover() {
                    Takeover::EndpointEnv(key) => key,
                    Takeover::HostRedirect => "",
                },
            )
            .output()
            .expect("node 应能跑起来");
        let _ = fs::remove_dir_all(&dir);
        assert!(
            out.status.success(),
            "node 退出非零：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some((port, String::from_utf8_lossy(&out.stdout).to_string()))
    }

    /// 国际版注入段的真实行为：族内改到本机（**SNI 保留**）、族外一个字段不碰。
    #[test]
    fn the_global_injection_actually_rewrites_connections() {
        let Some((port, out)) = observe_injection_with_node(Region::Global) else {
            return;
        };
        let cases = [
            // 族内：目标与端口都改到本机，servername 原样留给反代选上游；
            // 落地在回环上，CA 注入跟着生效（否则客户端认不出自签证书）
            ("family-sh", format!("QA family-sh host=127.0.0.1 hostname=127.0.0.1 port={port} servername=api3.qoder.sh ca=yes reject=false")),
            ("family-cn", format!("QA family-cn host=127.0.0.1 hostname=127.0.0.1 port={port} servername=api6.qoder.com.cn ca=yes reject=false")),
            // 只给 hostname 的调用形态：两条都要认（Node 的不同路径读的是其中一个）
            ("hostname-form", format!("QA hostname-form host=127.0.0.1 hostname=127.0.0.1 port={port} servername=api2.qoder.sh ca=yes reject=false")),
            // httpdns 的 resolved-ip 形态：host 已是解析出的 IP，域名只在 servername 里。
            // 这类连接必须照样改写，且 servername 原样保留给反代选上游 ——
            // 之前只测 host，整个对话流量都从这里漏出去
            ("resolved-ip", format!("QA resolved-ip host=127.0.0.1 hostname=127.0.0.1 port={port} servername=api3.qoder.sh ca=yes reject=false")),
            // 反向：host 是 IP、servername 族外 —— 一个字段都不能动
            // （`127.0.0.2` 在回环网段内，即使真的发起连接也是瞬时拒绝，不出网）
            ("resolved-ip-foreign-sni", "QA resolved-ip-foreign-sni host=127.0.0.2 hostname=undefined port=443 servername=example.com ca=no reject=undefined".into()),
            // 族外（业务面）：一个字段都不能动 —— 改写一旦漏出去就是「账号页也走反代」
            ("foreign-center", "QA foreign-center host=center.qoder.sh hostname=undefined port=443 servername=undefined ca=no reject=undefined".into()),
            ("foreign-openapi", "QA foreign-openapi host=openapi.qoder.sh hostname=undefined port=443 servername=undefined ca=no reject=undefined".into()),
            // 后缀攻击：`api3.qoder.sh.evil.com` 不在族内（族规则是整串锚定的）
            ("suffix-attack", "QA suffix-attack host=api3.qoder.sh.evil.com hostname=undefined port=443 servername=undefined ca=no reject=undefined".into()),
            // 「先在结尾凑出合法后缀」也不在族内：标签里不允许有点。这一例与
            // `region::is_model_gateway_host` 的同一例配对 —— 两侧判定必须一致
            ("double-tail", "QA double-tail host=api3.qoder.sh.qoder.sh hostname=undefined port=443 servername=undefined ca=no reject=undefined".into()),
            // 回环直连（反代自己再连、或客户端本来就写 127.0.0.1 的调用）
            ("loopback", format!("QA loopback host=127.0.0.1 hostname=undefined port={port} servername=undefined ca=yes reject=false")),
            ("localhost", format!("QA localhost host=localhost hostname=undefined port={port} servername=undefined ca=yes reject=false")),
        ];
        for (label, want) in cases {
            assert_eq!(qa_line(&out, label), want, "node 实际观测：\n{out}");
        }
    }

    /// 国内版注入段的真实行为：写 env 键、**不碰**连接目标、回环照样注 CA。
    #[test]
    fn the_cn_injection_actually_sets_the_endpoint_key() {
        let Some((port, out)) = observe_injection_with_node(Region::Cn) else {
            return;
        };
        assert_eq!(
            qa_line(&out, "env-key"),
            format!("QA env-key value=https://127.0.0.1:{port}"),
            "node 实际观测：\n{out}"
        );
        // 国内版靠 env 键让客户端自己连回环，去改 TLS 目标只是多一份没用的注入面
        assert_eq!(
            qa_line(&out, "family-sh"),
            "QA family-sh host=api3.qoder.sh hostname=undefined port=443 servername=undefined ca=no reject=undefined",
            "node 实际观测：\n{out}"
        );
        // 国内版不改 TLS：resolved-ip 形态（host=IP、servername=域名）同样一个字段都不碰
        assert_eq!(
            qa_line(&out, "resolved-ip"),
            "QA resolved-ip host=8.212.20.107 hostname=undefined port=443 servername=api3.qoder.sh ca=no reject=undefined",
            "node 实际观测：\n{out}"
        );
        assert_eq!(
            qa_line(&out, "loopback"),
            format!("QA loopback host=127.0.0.1 hostname=undefined port={port} servername=undefined ca=yes reject=false"),
            "node 实际观测：\n{out}"
        );
    }

    // ── 真机探针（默认 ignored：依赖这台机器上真的装了客户端） ─────────────

    /// 本机装的官方客户端必须能被解析到。
    ///
    /// 这条就是「Windows 上找不到客户端、报错却照着 macOS 说 /Applications」那个
    /// bug 的回归线 —— 上面所有用例都跑在临时沙箱里，谁也发现不了真实安装根写错了。
    /// `cargo test --lib -- --ignored the_installed_client_is_found`
    #[test]
    #[ignore]
    fn the_installed_client_is_found_on_this_machine() {
        let found: Vec<String> = Region::ALL
            .iter()
            .filter_map(|r| worker_path(*r).map(|p| format!("{} → {}", r.label(), p.display())))
            .collect();
        for line in &found {
            println!("{line}");
        }
        assert!(!found.is_empty(), "两个区域的客户端一个都没解析到");
    }

    /// 对**真实那份产物**做一次注入 → 校验 → 还原，逐字节比对。
    ///
    /// 假产物测不出真机上的三件事：文件权限、被别的进程占用、Windows 上 `rename`
    /// 顶替失败。只动国内版这一份（本机装了它），跑完必须还原成官方原样。
    /// `cargo test --lib -- --ignored a_real_injection_round_trip`
    #[test]
    #[ignore]
    fn a_real_injection_round_trip_leaves_the_artifact_byte_identical() {
        let Some(worker) = worker_path(Region::Cn) else {
            println!("本机没装国内版客户端，跳过");
            return;
        };
        let backup = backup_path(&worker);
        let backup_existed = backup.is_file();
        let before = fs::read_to_string(&worker).unwrap();
        assert_eq!(
            read_marker(&worker),
            None,
            "产物里已经有注入段（先关掉接管再跑这条）：{}",
            worker.display()
        );

        let url = "https://127.0.0.1:8789";
        assert!(install_at(&worker, Region::Cn, url, FAKE_CA).unwrap(), "首次注入应写盘");
        assert_eq!(read_marker(&worker).unwrap().url, url);
        assert!(uninstall_at(&worker).unwrap(), "摘除应写盘");
        assert_eq!(
            fs::read_to_string(&worker).unwrap(),
            before,
            "还原必须逐字节一致：{}",
            worker.display()
        );

        // 往返本身会留一份备份；它不该成为这台机器上的新垃圾（正式启用接管时
        // 应用会自己维护这一份，所以只清理「本来没有」的那种）。
        if !backup_existed {
            let _ = fs::remove_file(&backup);
        }
        println!("往返成功（逐字节还原）：{}", worker.display());
    }
}
