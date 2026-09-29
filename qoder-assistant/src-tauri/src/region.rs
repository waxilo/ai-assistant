//! 区域（Region）：Qoder 有**两套互不相通的部署**，本项目要同时支持。
//!
//! # 为什么必须把它抽成一个类型
//!
//! 上一版把域写成了四个 `pub const &str`（`qoder_api::AUTH_BASE` / `OPENAPI_BASE` /
//! `INFER_BASE`、`models::CATALOG_BASE`），路径与进程名另外散落在 `auth_file` /
//! `commands` / `stealth` / `netfix` 里。那在「只有一个域」的前提下还算收敛，
//! 一旦出现第二个区域就立刻变成**八处各写一半**：少改一处不会报错，
//! 只会表现成「这个功能在另一个区域上悄悄用错域」。
//!
//! 所以这里是**唯一**的区域事实表：域名、本地目录、官方客户端路径、进程正则
//! 全部按区域取，别处只消费、不自己拼。
//!
//! # 两套部署的实测事实（2026-09-19）
//!
//! | | 国际版 Global | 国内版 CN |
//! |---|---|---|
//! | `productId` | `qoder` | `qoder-cn` |
//! | 登录 / 授权 | `https://qoder.com` | `https://qoder.cn` |
//! | OpenAPI（额度 / 活动权益 / 用户 / COSY uid） | `https://openapi.qoder.sh` | `https://openapi.qoder.com.cn` |
//! | 模型网关（接管转发目标 **与** 模型目录） | `https://api2.qoder.sh`（选举后 `api3`） | `https://gateway.qoder.com.cn` |
//! | CLI 配置目录 | `~/.qoder` | `~/.qoder-cn` |
//! | 桌面端数据目录 | `com.qoder.app.stable` | `com.qodercn.app.stable` |
//! | macOS 应用 | `/Applications/Qoder.app` | `/Applications/Qoder CN.app` |
//! | Windows 安装目录 | `%LOCALAPPDATA%\Programs\Qoder` | `%LOCALAPPDATA%\Programs\Qoder CN` |
//! | 可执行名（`CFBundleExecutable`） | `Qoder` | `Qoder CN` |
//!
//! 证据来源：
//! - 两个 `app.asar` 里的 `environments.prod`（域）与 `cliConfigDirectoryName` /
//!   `cliEnvironmentPrefix`（目录）；
//! - 两个客户端的 `Info.plist`（`CFBundleIdentifier` / `CFBundleExecutable` /
//!   `CFBundleURLSchemes`）；
//! - **真机 CLI 日志**：`~/.qoder-cn/logs/runs/*/qodercli.log` 里实测出现的
//!   `https://gateway.qoder.com.cn/api/v2/model/list`、`https://openapi.qoder.com.cn/api/v1/userinfo`
//!   —— 国内版的模型目录确实不在 `api3` 上，而在 `gateway`；
//! - **国际版的网关族**（2026-09-29 实测）：客户端选举缓存
//!   `~/.qoder/.cache/endpoint-cache.json` 给出 `inference=api3.qoder.sh`、
//!   `security=api2.qoder.sh`、`fast=api6.qoder.com.cn`；未鉴权路由探测
//!   `https://api2|api3.qoder.sh/algo/api/v2/service/pro/sse/agent_chat_generation`
//!   与 `/algo/api/v2/model/list` 都有路由（SSE 回 200 内嵌 403 `Signature invalid`），
//!   而 `api2-v2.qoder.sh` 对这两条新路径**已经是 404**（只剩旧的
//!   `/model/v1/chat/completions`）—— 所以 `infer_base` 不再取它。
//!
//! # 两件事**不随区域变**
//!
//! 1. **`AUTH_CLIENT_ID` 完全相同**（两边 `authClientIds.prod` 逐字一致）；
//! 2. **授权链接一律不带 `redirect_uri`** —— 国内版的 `authRedirectUris.stable` 直接是
//!    `null`，官方自己就不用自定义 scheme 回调。这同时是 `oauth` 那条「不要照抄官方
//!    `redirect_uri`」结论的官方旁证。
//!
//! # 接管：两条路，别把参数互相借
//!
//! 国内版的 `cliEnvironmentPrefix` 是 `QODERCN`，端点键名由 `${prefix}${name}` 拼出来，
//! `QODERCN_SERVER_ENDPOINT` 已**实测生效**（客户端运行日志的 `[config-service] baseUrl`
//! 变成了我们给的值）—— 它覆盖**全部 purpose**，所以一份 env 键就够了。
//!
//! 国际版没有可用的键：`QODER_SERVER_ENDPOINT` 的读取者在国际版构建里恒不生效
//! （`if(!Ja) return`），`QODER_CENTER_ENDPOINT` 只覆盖 center。所以国际版改走
//! **TLS 目标重定向**：注入段把模型网关族（[`is_model_gateway_host`]）的连接目标
//! 改写成 `127.0.0.1:<port>` 并保留 SNI，反代按 SNI 选上游 —— 见 [`Region::takeover`]。

use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// 智能接管在**客户端侧**怎么把流量引到本机反代 —— 见 [`Region::takeover`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takeover {
    /// 国内版：写 `process.env[<键>] = https://127.0.0.1:<port>`，客户端自己把
    /// **全部 purpose** 的地址换成它。
    ///
    /// # 键名在产物里没有字面量，只能顺着 `Rr=` 回溯
    ///
    /// 客户端读端点的唯一入口是：
    ///
    /// ```text
    /// function v7a(){ if(!Ja) return; let A = process.env[aue]; let e = M7a(A); … }
    /// function yg(A){ return v7a() ?? A }      // 有覆盖就用覆盖，没有才退回默认
    /// ```
    ///
    /// 其中 `aue = Rr("SERVER_ENDPOINT")`、`Rr(name) = ${前缀}${name}`，前缀由区域
    /// 常量决定（国内版构建里 `Mo = "cn" == (eTs="cn")` 恒真，`"QODERCN_"` 被折叠进
    /// 产物）。**所以直接 grep `QODERCN_SERVER_ENDPOINT` 是 0 命中**（踩过这个坑），
    /// 要顺着 `Rr(` 与 `Uer=` 回溯才拿得到真实键名。
    ///
    /// # 覆盖值的形态（实测确认）
    ///
    /// `M7a()` 只做 `new URL(v).origin` 校验，所以：
    ///
    /// - **必须 https**（没有 `http:` 分支）；
    /// - 路径必须为空或 `/`，不能带查询串 / 散列 / 用户名；
    /// - **端口可以带** —— origin 含端口，`https://127.0.0.1:8789` 合法，
    ///   不必占 443、不需要管理员。反代因此必须自己终止 TLS，见 [`crate::certs`]。
    ///
    /// 实测（`QODERCN_SERVER_ENDPOINT=https://127.0.0.1:9999` 手工起 worker）：
    /// 客户端日志 `[config-service] baseUrl` 立刻由 `'(SDK default, …)'` 变成该值。
    EndpointEnv(&'static str),
    /// 国际版：**没有**可覆盖的键（`v7a()` 那句 `if(!Ja) return` 让它在国际版构建里
    /// 恒不生效，`QODER_CENTER_ENDPOINT` 只覆盖 center），所以改在 `tls.connect` 里
    /// 把模型网关族的连接目标改写成 `127.0.0.1:<port>`。
    ///
    /// # 为什么不是「覆盖 center + 代答选举」
    ///
    /// 那条路要在反代里区分 center 与推理两类流量，而它们的路径**是重叠的**
    /// （`/api/v2/service/pro/*` 两边都在用），且改写后 Host/SNI 全变成 127.0.0.1，
    /// 没有可靠判据 —— 判错的代价是打断用户的对话。
    ///
    /// # 为什么保留 SNI
    ///
    /// 选举会把推理域在 `api1/api2/api3/api6…` 之间换，写死一个上游就会
    /// 「今天能用、明天 404」。所以注入段**只改 TCP 目标、保留 `servername`**，
    /// 反代握手后读 SNI 选上游（[`is_model_gateway_host`] 限定族）。
    /// center / openapi **不重定向**：它们是业务面（选举、策略、tracking），
    /// 官方直连即可，也避免把非推理流量卷进换号签名。
    HostRedirect,
}

/// 网关族规则的**唯一来源**：反代（[`is_model_gateway_host`]，手判、不引正则依赖）
/// 与注入段（`patch::render` 把它原样嵌成客户端里的 `new RegExp(...)`）都取这一份。
///
/// 分成两处手写就会出现「注入改了、反代不认」：客户端连上来、反代却回落到
/// `infer_base`，于是选举到 api3 的机器全部打到 api2 上 —— 表现是接管静默失效，
/// 不像报错那样容易发现。
pub const MODEL_GATEWAY_HOST_REGEX: &str = "^api[0-9a-z-]*\\.qoder\\.(sh|com\\.cn)$";

/// 国际版模型网关族：`api*.qoder.sh` 与 `api*.qoder.com.cn`。
///
/// 不含 `center.qoder.sh` / `openapi.qoder.sh` / `gateway.qoder.com.cn`（国内版的网关，
/// 国际版客户端不会连它）；也不接受任何其它域 —— 反代只按这个族决定「要不要拿 SNI
/// 当上游」，多认一个域就等于把本机变成一个转发器。
///
/// 规则**逐字**在 [`MODEL_GATEWAY_HOST_REGEX`] 里，反代与注入段（客户端侧）
/// 都从那里取，别在任一侧另写一份 —— 也**别在这里另立判法**：这条手判必须与那条
/// 正则逐字等价，宽一格窄一格都是一样的故障（注入改了、反代不认，或反过来）。
///
/// 它曾按「域后缀结尾」判，于是 `api3.qoder.sh.qoder.sh` 这类**多标签**主机被放进来
/// （正则从不接受），也就是反代比注入段宽 —— 族外的 SNI 本该一律 421 拒掉。
pub fn is_model_gateway_host(host: &str) -> bool {
    let Some(rest) = host.strip_prefix("api") else {
        return false;
    };
    // 剥掉 `api` 与两个合法后缀，剩下的必须是**单个不含点的标签**（对应正则里的
    // `[0-9a-z-]*`，它的字符类里没有 `.`）—— 空标签合法（`api.qoder.sh`）。
    // `api3.qoder.sh.evil.com` 在这里就因为后缀对不上而落选；`api3.qoder.sh.qoder.sh`
    // 则是因为剩下的标签里含点。两种漏网形态各有一例回归用例（见 tests）。
    let label = rest
        .strip_suffix(".qoder.sh")
        .or_else(|| rest.strip_suffix(".qoder.com.cn"));
    let Some(label) = label else {
        return false;
    };
    label
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Qoder 的部署区域。新增区域只需往这里加一个变体 + 补全下面四组常量。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    /// 国际版：`qoder.com` / `openapi.qoder.sh`
    #[default]
    Global,
    /// 国内版：`qoder.cn` / `openapi.qoder.com.cn`
    Cn,
}

impl Region {
    /// 全部区域（界面上的顺序就是它）—— 国内版在前、作为界面默认选中的那一项。
    /// 注意这里的顺序只影响**展示/兜底**，不是 [`Region::default`]：后者是账户
    /// region 字段的 serde 缺省（老账号无该字段 = 国际版），不能跟着调。
    pub const ALL: [Region; 2] = [Region::Cn, Region::Global];

    /// 稳定标识：落盘（`accounts.json` / `settings.json`）与 IPC 都用它。
    ///
    /// **不要**改这几个字符串 —— 它就是持久化格式，改名等于把用户已有的账号
    /// 全部降级成「区域未知」。
    pub fn key(self) -> &'static str {
        match self {
            Region::Global => "global",
            Region::Cn => "cn",
        }
    }

    /// 界面用中文名。用在账号标签、登录弹窗、接管页。
    pub fn label(self) -> &'static str {
        match self {
            Region::Global => "国际版",
            Region::Cn => "国内版",
        }
    }

    /// 一句话说明，界面在需要解释差异时用它（不要在每个 UI 里自己编）。
    pub fn hint(self) -> &'static str {
        match self {
            Region::Global => "qoder.com（国际版，OpenAPI 在 openapi.qoder.sh）",
            Region::Cn => "qoder.cn（国内版，OpenAPI 在 openapi.qoder.com.cn）",
        }
    }

    // ---------------------------------------------------------------- 域名

    /// 登录 / 设备授权流的基址（`environments.prod.authBaseUrl`）。
    pub fn auth_base(self) -> &'static str {
        match self {
            Region::Global => "https://qoder.com",
            Region::Cn => "https://qoder.cn",
        }
    }

    /// OpenAPI 基址：额度、活动权益（签到）、用户信息、token 续签全在这里
    /// （`environments.prod.openApiBaseUrl`）。
    pub fn openapi_base(self) -> &'static str {
        match self {
            Region::Global => "https://openapi.qoder.sh",
            Region::Cn => "https://openapi.qoder.com.cn",
        }
    }

    /// 模型网关：接管反代把 CLI 的对话请求转发到它，**模型目录也在同一个域**
    /// （`GET {infer_base}/algo/api/v2/model/list`，见 `models` 模块头）。
    ///
    /// 国际版取**客户端自己的默认推理域** `api2.qoder.sh`（`qgd()` 的 prod 默认；
    /// 选举之后实际跑的是 `api3.qoder.sh`）。这里曾经写 `api2-v2.qoder.sh` —— 那是
    /// 只服务旧 `/model/v1/chat/completions` 的老网关，**对客户端现在用的
    /// `/algo/api/v2/...` 两条路径都已经是 404**（2026-09-29 未鉴权探测）。
    ///
    /// 接管生效时这个值只是**兜底**：反代优先按客户端握手里的 SNI 选上游
    /// （选举会把域换成 api1/api3/api6…），只有拿不到 SNI 才回落到这里。
    pub fn infer_base(self) -> &'static str {
        match self {
            Region::Global => "https://api2.qoder.sh",
            Region::Cn => "https://gateway.qoder.com.cn",
        }
    }

    /// COSY 身份查询的基址：`GET {identity_base}/api/v3/user/status`（见 [`crate::cosy`]）。
    ///
    /// 两个区域**不在一起**，别顺手统一：
    /// - 国内版与业务同域（`gateway.qoder.com.cn`，本机 `cosy_uid` 就是这么补齐的）；
    /// - 国际版只在 OpenAPI 上 —— 官方客户端自己打的就是
    ///   `https://openapi.qoder.sh/api/v3/user/status`（`~/.qoder/logs` 里 510 条，
    ///   200 OK），而 `api3.qoder.sh` / `api2-v2.qoder.sh` 上这条路径都是 404。
    ///   查询基址指错的表现是「uid 永远补不上 ⇒ 换号签名永远做不成」，
    ///   界面却只显示「接管已开启」。
    pub fn identity_base(self) -> &'static str {
        match self {
            Region::Global => "https://openapi.qoder.sh",
            Region::Cn => "https://gateway.qoder.com.cn",
        }
    }

    /// 登录用的公开 client id。**两个区域实测完全相同**，所以它不是区域差异项；
    /// 放在这里只是为了让「要拼登录 URL 的人」不必再去别处找。
    pub const AUTH_CLIENT_ID: &'static str = "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa";

    // ---------------------------------------------------------------- 本地目录

    /// CLI 配置目录名（家目录下）：`~/.qoder` / `~/.qoder-cn`
    /// （`cliConfigDirectoryName`）。
    ///
    /// **只有它**，没有「直接用 `dirs::home_dir()` 拼出来」的兄弟方法：家目录一律由
    /// 调用方传入。这样测试能把它指向临时目录，而「能不能被测」正是这类
    /// 「往别人的目录里写文件」的代码最需要的属性。
    pub fn cli_dir_name(self) -> &'static str {
        match self {
            Region::Global => ".qoder",
            Region::Cn => ".qoder-cn",
        }
    }

    /// 桌面端数据目录名（macOS `~/Library/Application Support/<名>`、
    /// Windows `%APPDATA%\<名>`）—— 两边用的是同一个目录名。
    pub fn profile_dir_name(self) -> &'static str {
        match self {
            Region::Global => "com.qoder.app.stable",
            Region::Cn => "com.qodercn.app.stable",
        }
    }

    /// 桌面端数据目录的候选路径（按平台）。
    ///
    /// 目录里那个 `auth.v1.dat` 是 Chromium `safeStorage`（OSCrypt）写出来的，
    /// 两个平台的密钥通道不同、但**都能端外解**：Windows 走 DPAPI，
    /// macOS 走钥匙串（见 [`Region::keychain_service`] 与 [`crate::auth_file`]）。
    pub fn profile_dirs(self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        #[cfg(target_os = "windows")]
        {
            if let Some(roaming) = dirs::data_dir() {
                out.push(roaming.join(self.profile_dir_name()));
            }
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(home) = dirs::home_dir() {
                out.push(
                    home.join("Library")
                        .join("Application Support")
                        .join(self.profile_dir_name()),
                );
            }
        }
        let _ = &mut out;
        out
    }

    // ---------------------------------------------------------------- 本机凭据

    /// macOS 钥匙串里 `safeStorage` 主密码的**服务名**。
    ///
    /// ⚠️ 实测字面量，**不要**图省事按 [`Region::app_name`] 拼：`app_name()` 是
    /// `Qoder` / `Qoder CN`（`CFBundleExecutable`，`open -a` 认的那个名字），
    /// 而钥匙串里是 `Qoder App Safe Storage` / `Qoder CN App Safe Storage` —— **多一个 `App`**。
    /// 这个名字来自 Electron 侧的产品名，`Info.plist` 的 `CFBundleName` / `CFBundleDisplayName`
    /// 里都没有它（两边分别是 `Qoder` / `Qoder CN`），所以只能按区域钉死。
    ///
    /// 拼错的后果不报错：`security` 找不到条目 → 与「那个版本没登录」长得一模一样。
    pub fn keychain_service(self) -> &'static str {
        match self {
            Region::Global => "Qoder App Safe Storage",
            Region::Cn => "Qoder CN App Safe Storage",
        }
    }

    /// 同上，钥匙串条目的**账号名**（实测 = 服务名去掉 ` Safe Storage` 再加 ` Key`）。
    pub fn keychain_account(self) -> &'static str {
        match self {
            Region::Global => "Qoder App Key",
            Region::Cn => "Qoder CN App Key",
        }
    }

    // ---------------------------------------------------------------- 官方客户端（无进程指纹）

    // 这里曾有 `app_name` / `macos_app_dir` / `macos_exec_path` / `macos_process_pattern`
    // 一串「进程指纹」，服务的是「退出客户端 → 重启客户端 → 判断它在不在跑」那套操作。
    // 那套操作整体删除之后（Qoder 的推理是**每次会话一次性 spawn 的 `--print` 进程**，
    // 没有可重启的长驻 host），这四个方法就没有调用方了，一并删掉 —— 留着只会让人
    // 以为本应用还会去动客户端进程。
    //
    // 顺带记一笔当年的坑：老正则写死成 `.../Contents/MacOS/Electron`，而两个客户端的
    // `CFBundleExecutable` 其实是 `Qoder` / `Qoder CN` —— 那条正则**一个进程都匹配不到**，
    // 于是「重启成功」这件事长期是假装做完了。这也是为什么「假装接管成功」能骗过界面。

    // ------------------------------------------------------------------ 智能接管

    /// 智能接管在**客户端侧**的落点。两个区域走的是两条不同的路，别把参数互相借。
    pub fn takeover(self) -> Takeover {
        match self {
            Region::Cn => Takeover::EndpointEnv("QODERCN_SERVER_ENDPOINT"),
            Region::Global => Takeover::HostRedirect,
        }
    }

    /// 官方客户端在本机叫什么 —— 安装目录名（Windows）与应用包名（macOS 去掉 `.app`）
    /// 用的是同一个名字，也用在「重启客户端」这类面向用户的话术里。
    pub fn client_name(self) -> &'static str {
        match self {
            Region::Global => "Qoder",
            Region::Cn => "Qoder CN",
        }
    }

    /// macOS 应用包路径。
    pub fn macos_app_dir(self) -> &'static str {
        match self {
            Region::Global => "/Applications/Qoder.app",
            Region::Cn => "/Applications/Qoder CN.app",
        }
    }

    /// 官方客户端的**安装根候选**（按探测顺序）。
    ///
    /// ⚠️ 这里曾经只有一个写死的 `/Applications/<名>.app`，于是 Windows 上
    /// **永远找不到客户端**：引擎在装端点之前就放弃，界面端出来的却是一句
    /// 「请检查端口是否被占用、以及官方客户端是否已安装在 /Applications」——
    /// 两句在 Windows 上都是错的，用户只能干瞪眼。
    ///
    /// 平台差异**到此为止**：往下 `…/app.asar.unpacked/node_modules/@qoder-ai/<sdk>`
    /// 两边逐字相同 —— Windows 那份的相对路径取自客户端自己的 `fast-update` 清单
    /// （`resources/app.asar.unpacked/…`），macOS 那份在应用包内的 `Contents/Resources` 下。
    pub fn client_install_dirs(self) -> Vec<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            vec![PathBuf::from(self.macos_app_dir())]
        }
        #[cfg(target_os = "windows")]
        {
            let name = self.client_name();
            let mut out = Vec::new();
            // 默认是 per-user 安装（客户端 `resources/install-type.json` 里就是 "user"）：
            // `%LOCALAPPDATA%\Programs\<名>`。装到 Program Files 的是全机安装。
            if let Some(local) = dirs::data_local_dir() {
                out.push(local.join("Programs").join(name));
            }
            if let Some(pf) = std::env::var_os("ProgramFiles") {
                out.push(PathBuf::from(pf).join(name));
            }
            out
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Vec::new()
        }
    }

    /// `resources` 那一层在安装根里的位置。
    ///
    /// macOS 在应用包内叫 `Contents/Resources`，Windows 直接就是安装目录下的
    /// `resources` —— 这是两个平台**唯一**的路径差异。
    fn resources_dir() -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            PathBuf::from("Contents").join("Resources")
        }
        #[cfg(not(target_os = "macos"))]
        {
            PathBuf::from("resources")
        }
    }

    /// 智能接管的落点候选（按探测顺序）：官方 agent SDK 在 `app.asar.unpacked` 里的根目录。
    ///
    /// # 为什么是 asar 外这份
    ///
    /// 客户端起推理进程前会把路径里的 `app.asar` 换成 `app.asar.unpacked`
    /// （产物里的 `xt()`），命中就直接用，并留下诊断
    /// `[WorkerTransport] Using asar-unpacked worker runtime: …`。
    /// 也就是说**被执行的正是 asar 之外那一份** —— 它不在归档完整性校验范围内，
    /// 可以直接改；而 asar 内那份永远轮不到。
    ///
    /// # 顺序：fast-update 的版本目录在前，顶层垫后
    ///
    /// 客户端有 fast-update：更新解到安装根下的 `.qoder-versions/<版本>/`，
    /// 之后**实际执行的是版本目录里那份**（2026-09-24 真机实测：顶层
    /// `resources` 与 `.qoder-versions/0.3.4` 并存，桌面日志的
    /// `[WorkerTransport]` 行指向版本目录那份）；顶层那份是首次安装的原件，
    /// 只在还没 fast-update 过时才被执行。所以版本目录**优先**、顶层**垫底**；
    /// 具体注入哪几份由 [`crate::patch`] 决定（存在的全量注入）。
    pub fn worker_sdk_roots(self) -> Vec<PathBuf> {
        let sdk = match self {
            Region::Global => "qoder-agent-sdk",
            Region::Cn => "qoder-cn-agent-sdk",
        };
        let mut out = Vec::new();
        for root in self.client_install_dirs() {
            out.extend(versioned_sdk_dirs(&root, sdk));
            out.push(
                root.join(Self::resources_dir())
                    .join("app.asar.unpacked")
                    .join("node_modules")
                    .join("@qoder-ai")
                    .join(sdk),
            );
        }
        out
    }

    /// 主落点：探测顺序里第一个**真的存在**的候选；一个都不在时给第一个 ——
    /// 让「客户端没装」的报错指向最可能的那个路径，而不是一句无从下手的「没装」。
    pub fn worker_sdk_root(self) -> Option<PathBuf> {
        let roots = self.worker_sdk_roots();
        roots
            .iter()
            .find(|p| p.is_dir())
            .cloned()
            .or_else(|| roots.into_iter().next())
    }

    /// 报错文案里那句「客户端该装在哪」：把候选路径用「或」连起来。
    pub fn install_hint(self) -> String {
        let dirs = self.client_install_dirs();
        if dirs.is_empty() {
            return "官方客户端的默认安装位置".to_string();
        }
        dirs.iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" 或 ")
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// fast-update 的版本目录（`<安装根>/.qoder-versions/<版本>/`）里的 SDK 根，
/// **按版本从高到低**排；目录不存在或没有版本目录时返回空。
///
/// 只挑「名字能解析成 semver」的子目录 —— `*.qoder-update-ready.json` 这类
/// 同级文件、以及将来可能出现的非版本目录都天然被排除。
///
/// ⚠️ 「选最高版本」≠「应用正在跑的那份」：本机实测 0.4.1 已 stage 而应用仍跑
/// 0.3.4。所以这里把**所有**版本目录都列出来交给上层全量注入（见 [`crate::patch`]），
/// 排序只为让「主落点」（报错文案 / 提示用）尽量贴近最新。
fn versioned_sdk_dirs(install_root: &Path, sdk: &str) -> Vec<PathBuf> {
    let root = install_root.join(".qoder-versions");
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut versions: Vec<(SemVer, PathBuf)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().to_string();
            Some((semver(&name)?, p))
        })
        .collect();
    versions.sort_by(|a, b| b.0.cmp(&a.0));
    versions
        .into_iter()
        .map(|(_, v)| {
            v.join(Region::resources_dir())
                .join("app.asar.unpacked")
                .join("node_modules")
                .join("@qoder-ai")
                .join(sdk)
        })
        .collect()
}

/// 解析 `X.Y.Z` 形式的版本号（客户端 fast-update 目录名就是这种形态）。
/// 解析不出的名字返回 None —— 宁可少认一个目录，也不把乱七八糟的名字排进去。
fn semver(name: &str) -> Option<SemVer> {
    let mut it = name.split('.');
    let parse = |s: &str| s.parse::<u64>().ok();
    Some((
        parse(it.next()?)?,
        parse(it.next()?)?,
        parse(it.next()?)?,
    ))
}

/// `(major, minor, patch)`，排序用。
type SemVer = (u64, u64, u64);

#[cfg(test)]
mod tests {
    use super::*;

    /// 两块域的取值必须各就各位 —— 这条断言是「域可切换」的地基：
    /// 任何一处把 CN 的域写成 Global 的，都会让另一个区域的请求悄悄打到错的域上
    /// （表现是 401 / 404，而不是崩溃）。
    #[test]
    fn each_region_carries_its_own_domains() {
        assert_eq!(Region::Global.auth_base(), "https://qoder.com");
        assert_eq!(Region::Cn.auth_base(), "https://qoder.cn");
        assert_eq!(Region::Global.openapi_base(), "https://openapi.qoder.sh");
        assert_eq!(Region::Cn.openapi_base(), "https://openapi.qoder.com.cn");
        assert_eq!(Region::Cn.infer_base(), "https://gateway.qoder.com.cn");
        assert_eq!(Region::Global.infer_base(), "https://api2.qoder.sh");
        // 国际版的推理网关（模型目录也在它上面）与 openapi 不是同一个域，别顺手统一
        assert_ne!(Region::Global.infer_base(), Region::Global.openapi_base());
        // COSY 身份查询（uid）与推理**不在一个域**上：国际版的 uid 只在 openapi 上
        // （客户端日志实测 `openapi.qoder.sh/api/v3/user/status` 200），指到网关会 404。
        assert_eq!(
            Region::Global.identity_base(),
            "https://openapi.qoder.sh"
        );
        assert_eq!(Region::Cn.identity_base(), "https://gateway.qoder.com.cn");
        // 国内版恰好与业务同域（本机 cosy_uid 就是这么补齐的）；国际版不同域，见上
        assert_eq!(Region::Cn.identity_base(), Region::Cn.infer_base());
    }

    /// 本地目录：CLI 目录与桌面端数据目录都必须按区域分开。
    /// 混用的后果是「在 A 区域账号上写下 B 区域的配置」——不报错，但接管静默失效。
    #[test]
    fn each_region_carries_its_own_local_directories() {
        assert_eq!(Region::Global.cli_dir_name(), ".qoder");
        assert_eq!(Region::Cn.cli_dir_name(), ".qoder-cn");
        assert_eq!(Region::Global.profile_dir_name(), "com.qoder.app.stable");
        assert_eq!(Region::Cn.profile_dir_name(), "com.qodercn.app.stable");
        assert_ne!(Region::Global.cli_dir_name(), Region::Cn.cli_dir_name());
    }

    /// 钥匙串条目名必须与实测逐字一致：
    /// `security find-generic-password -s "Qoder CN App Safe Storage" -a "Qoder CN App Key" -w`
    /// 能直接取到密码。拼错的唯一表现是「读不到本机凭据」——不报错、不崩溃，
    /// 所以只能靠这条断言钉住。
    #[test]
    fn keychain_item_names_match_the_real_ones() {
        assert_eq!(Region::Global.keychain_service(), "Qoder App Safe Storage");
        assert_eq!(Region::Global.keychain_account(), "Qoder App Key");
        assert_eq!(Region::Cn.keychain_service(), "Qoder CN App Safe Storage");
        assert_eq!(Region::Cn.keychain_account(), "Qoder CN App Key");
        // 两套部署的钥匙串条目也必须是两条：共用一条就会拿 A 的密钥去解 B 的密文
        assert_ne!(
            Region::Global.keychain_service(),
            Region::Cn.keychain_service()
        );
        // 回归护栏：这里**曾经**按 `<客户端名> Safe Storage` 拼（少一个 `App`），
        // 实测 `Qoder Safe Storage` 根本不存在。这条断言让那种"顺手统一"改法当场失败
        // ——拼错的唯一表现是「读不到本机凭据」，不报错不崩溃，只能靠断言钉住。
        for (svc, naive) in [
            (Region::Global.keychain_service(), "Qoder Safe Storage"),
            (Region::Cn.keychain_service(), "Qoder CN Safe Storage"),
        ] {
            assert_ne!(svc, naive, "钥匙串名不是由客户端名直接拼出来的");
        }
    }

    /// 落盘标识就是持久化格式：改名 = 老账号全部变「区域未知」。
    #[test]
    fn keys_are_the_persistence_format() {
        assert_eq!(Region::Global.key(), "global");
        assert_eq!(Region::Cn.key(), "cn");
        let json = serde_json::to_string(&Region::Cn).unwrap();
        assert_eq!(json, "\"cn\"");
        assert_eq!(
            serde_json::from_str::<Region>("\"global\"").unwrap(),
            Region::Global
        );
    }

    /// 缺省必须是国际版：老 `accounts.json` 里没有 `region` 字段，
    /// 那些账号全部来自国际版域。
    #[test]
    fn default_region_is_global() {
        assert_eq!(Region::default(), Region::Global);
    }

    /// 接管落点：国内版是 env 键、国际版是 TLS 重定向 —— 两条路，别把参数互相借。
    ///
    /// 国际版**没有**可用的键：`QODER_SERVER_ENDPOINT` 的读取者在国际版构建里恒不生效
    /// （`if(!Ja) return`），覆盖键只覆盖 center。所以它不能退成 `EndpointEnv`，
    /// 否则等于写一段没人读的注入、界面却显示「接管已开启」。
    #[test]
    fn takeover_route_is_the_one_each_client_actually_reads() {
        assert_eq!(
            Region::Cn.takeover(),
            Takeover::EndpointEnv("QODERCN_SERVER_ENDPOINT")
        );
        assert_eq!(Region::Global.takeover(), Takeover::HostRedirect);
        // 键名前缀与区域一一对应（产物里 `Rr(name) = ${前缀}${name}`）：读错区域的键
        // 不会报错，只会让覆盖静默失效
        match Region::Cn.takeover() {
            Takeover::EndpointEnv(key) => assert!(key.starts_with("QODERCN_"), "{key}"),
            other => panic!("国内版应走 env 覆盖，实际 {other:?}"),
        }
        assert!(
            !matches!(Region::Global.takeover(), Takeover::EndpointEnv(_)),
            "国际版不能退回 env 覆盖"
        );
    }

    /// 只有模型网关族允许被重定向 —— 反代按这条规则决定「要不要拿 SNI 当上游」，
    /// 多认一个域就等于把本机变成一个开放转发器。
    ///
    /// ⚠️ 与 `patch::render` 里嵌入的 JS 正则（`^api[0-9a-z-]*\.qoder\.(sh|com\.cn)$`）
    /// 同一条规则：两处的用例表必须一致，否则会出现「注入改了、反代不认」的静默失效。
    #[test]
    fn only_the_model_gateway_family_may_be_redirected() {
        for ok in [
            "api.qoder.sh",
            "api1.qoder.sh",
            "api2.qoder.sh",
            "api3.qoder.sh",
            "api2-v2.qoder.sh",
            "api.qoder.com.cn",
            "api6.qoder.com.cn",
        ] {
            assert!(is_model_gateway_host(ok), "{ok} 应在网关族内");
        }
        for no in [
            // 业务面（选举 / 策略 / 用户 / 国内版网关）——官方直连，不进接管
            "center.qoder.sh",
            "openapi.qoder.sh",
            "openapi.qoder.com.cn",
            "gateway.qoder.com.cn",
            "qoder.sh",
            "qoder.com",
            "www.qoder.com",
            // 「域是后缀、真实域在别处」的漏网形态
            "api3.qoder.sh.evil.com",
            "api.qoder.shx",
            "api3.qoder.com.cn.evil.com",
            // 「先在结尾凑出合法后缀、真正的主机名在中间」：只按后缀结尾判会放进来，
            // 而注入段那条正则从不接受（它的字符类里没有点）—— 反代宽一格 = 421 形同虚设
            "api3.qoder.sh.qoder.sh",
            "api3.qoder.com.cn.qoder.com.cn",
            // 族内主机名不允许大写 / 下划线
            "API3.qoder.sh",
            "api_3.qoder.sh",
            // 别的产品域
            "apiv2.qoder.cn",
            // 空串 / 本机名这类杂音
            "",
            "127.0.0.1",
            "localhost",
            "api",
            "api.",
        ] {
            assert!(!is_model_gateway_host(no), "{no} 不该被当成网关族");
        }
    }

    /// 接管落点必须指向 asar **之外**那份被执行的产物，并且两个客户端各指各的 SDK。
    ///
    /// 两个平台的安装根不同（macOS 在应用包内、Windows 在 `%LOCALAPPDATA%\Programs`），
    /// 而**往下逐字相同** —— 所以共同的不变量两边都查，各自的根按平台钉住。
    /// 这条曾经只按 macOS 写（`/Applications`），于是 Windows 上的接管永远找不到
    /// 客户端、报错还照着 macOS 说「装在 /Applications」。
    #[test]
    fn takeover_target_points_at_the_unpacked_sdk_of_each_client() {
        // 路径里的分隔符统一成 `/`：断言只关心层级，不关心平台写法
        fn norm(p: PathBuf) -> String {
            p.to_string_lossy().replace('\\', "/")
        }
        let cn = norm(Region::Cn.worker_sdk_root().unwrap());
        let g = norm(Region::Global.worker_sdk_root().unwrap());

        // 关键不变量：路径里必须是 `app.asar.unpacked`，不能落在 `app.asar` 内
        // （asar 内那份不受我们控制，也不会被执行）
        for s in [&cn, &g] {
            assert!(s.contains("/app.asar.unpacked/"), "{s}");
            assert!(!s.contains("/app.asar/node_modules"), "{s}");
        }
        // 两个客户端各指各的 SDK，混了就是「把端点装到另一个客户端上」（不报错、只空转）
        assert!(
            cn.ends_with("/node_modules/@qoder-ai/qoder-cn-agent-sdk"),
            "国内版要指 cn 那份 SDK：{cn}"
        );
        assert!(
            g.ends_with("/node_modules/@qoder-ai/qoder-agent-sdk"),
            "国际版要指非 cn 那份 SDK：{g}"
        );
        // 两个客户端的安装根不能是同一个
        assert_ne!(cn, g);

        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                cn,
                "/Applications/Qoder CN.app/Contents/Resources/app.asar.unpacked/node_modules/@qoder-ai/qoder-cn-agent-sdk"
            );
            assert_eq!(
                g,
                "/Applications/Qoder.app/Contents/Resources/app.asar.unpacked/node_modules/@qoder-ai/qoder-agent-sdk"
            );
        }
        #[cfg(target_os = "windows")]
        {
            // 默认 per-user 安装：`%LOCALAPPDATA%\Programs\<名>`。
            // ⚠️ fast-update 之后主落点可能落在 `.qoder-versions/<版本>/` 里
            // （见 [`Region::worker_sdk_roots`]），所以这里只钉「安装根 + SDK 包名」，
            // 不钉 `resources` 那一层 —— 本机实测两份并存，应用跑的是版本目录那份。
            assert!(cn.contains("/Programs/Qoder CN/"), "{cn}");
            assert!(g.contains("/Programs/Qoder/"), "{g}");
        }
    }

    /// fast-update 版本目录必须按版本**从高到低**排，且非版本名被排除。
    ///
    /// 这条守的是「主落点尽量贴近最新」：0.10 排在 0.9 前面（字符串序会排反），
    /// `*.qoder-update-ready.json` 这类同名文件不该被当成版本目录。
    #[test]
    fn versioned_sdk_dirs_are_sorted_newest_first() {
        let dir = std::env::temp_dir().join(format!(
            "qa-region-versions-{}",
            uuid::Uuid::new_v4()
        ));
        let root = dir.join("Qoder");
        for v in ["0.3.3", "0.3.4", "0.10.0", "0.9.2"] {
            fs::create_dir_all(root.join(".qoder-versions").join(v)).unwrap();
        }
        // 同级的非版本文件与目录：都必须被排除
        fs::write(
            root.join(".qoder-versions").join("0.3.4.qoder-update-ready.json"),
            b"{}",
        )
        .unwrap();
        fs::create_dir_all(root.join(".qoder-versions").join("junk")).unwrap();

        let got = versioned_sdk_dirs(&root, "qoder-agent-sdk");
        let names: Vec<String> = got
            .iter()
            .map(|p| {
                p.to_string_lossy()
                    .replace('\\', "/")
                    .split("/.qoder-versions/")
                    .nth(1)
                    .unwrap()
                    .split('/')
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            vec!["0.10.0", "0.9.2", "0.3.4", "0.3.3"],
            "版本要从高到低：{names:?}"
        );
        // 每条都要指到 SDK 根：resources 层按平台、往下逐字相同
        for p in &got {
            let s = p.to_string_lossy().replace('\\', "/");
            assert!(s.ends_with("/app.asar.unpacked/node_modules/@qoder-ai/qoder-agent-sdk"), "{s}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// 没有 `.qoder-versions` 时（CN 版当前如此）就一个版本候选都不给，
    /// 落点自然回到顶层 —— 绝不能因为扫描失败而凭空造路径。
    #[test]
    fn versioned_sdk_dirs_are_empty_without_the_versions_root() {
        let dir = std::env::temp_dir().join(format!(
            "qa-region-noversions-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        assert!(versioned_sdk_dirs(&dir, "qoder-cn-agent-sdk").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// 「客户端该装在哪」这句话必须按平台说：Windows 上照 macOS 说 `/Applications`
    /// 会把用户送去一个在这台机器上根本不存在的地方。
    #[test]
    fn install_hint_names_the_platforms_real_install_root() {
        let hint = Region::Cn.install_hint();
        #[cfg(target_os = "macos")]
        assert!(hint.contains("/Applications/Qoder CN.app"), "{hint}");
        #[cfg(target_os = "windows")]
        assert!(
            hint.contains("Programs") && hint.contains("Qoder CN"),
            "Windows 的提示要指向安装目录：{hint}"
        );
        assert!(hint.contains("Qoder CN"), "要分区域点名：{hint}");
    }
}
