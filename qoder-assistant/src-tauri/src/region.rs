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
//! | OpenAPI（额度 / 活动权益 / 用户） | `https://openapi.qoder.sh` | `https://openapi.qoder.com.cn` |
//! | 模型网关（接管转发目标 **与** 模型目录） | `https://api2-v2.qoder.sh` | `https://gateway.qoder.com.cn` |
//! | CLI 配置目录 | `~/.qoder` | `~/.qoder-cn` |
//! | 桌面端数据目录 | `com.qoder.app.stable` | `com.qodercn.app.stable` |
//! | macOS 应用 | `/Applications/Qoder.app` | `/Applications/Qoder CN.app` |
//! | 可执行名（`CFBundleExecutable`） | `Qoder` | `Qoder CN` |
//!
//! 证据来源：
//! - 两个 `app.asar` 里的 `environments.prod`（域）与 `cliConfigDirectoryName` /
//!   `cliEnvironmentPrefix`（目录）；
//! - 两个客户端的 `Info.plist`（`CFBundleIdentifier` / `CFBundleExecutable` /
//!   `CFBundleURLSchemes`）；
//! - **真机 CLI 日志**：`~/.qoder-cn/logs/runs/*/qodercli.log` 里实测出现的
//!   `https://gateway.qoder.com.cn/api/v2/model/list`、`https://openapi.qoder.com.cn/api/v1/userinfo`
//!   —— 国内版的模型目录确实不在 `api3` 上，而在 `gateway`。
//!
//! # 两件事**不随区域变**
//!
//! 1. **`AUTH_CLIENT_ID` 完全相同**（两边 `authClientIds.prod` 逐字一致）；
//! 2. **授权链接一律不带 `redirect_uri`** —— 国内版的 `authRedirectUris.stable` 直接是
//!    `null`，官方自己就不用自定义 scheme 回调。这同时是 `oauth` 那条「不要照抄官方
//!    `redirect_uri`」结论的官方旁证。
//!
//! # 不猜的部分
//!
//! 国内版的 `authRedirectUris` 是 `null`、`cliEnvironmentPrefix` 是 `QODERCN`，
//! 键名由 `${prefix}${name}` 拼出来。`QODERCN_SERVER_ENDPOINT` 已**实测生效**
//! （客户端运行日志的 `[config-service] baseUrl` 变成了我们给的值），所以
//! [`Region::endpoint_env_key`] 对国内版返回它；国际版走的是只覆盖 center 的另一个键，
//! 推理端点还得靠代答 `/api/v3/service/region/endpoints`，因此**返回 `None` 显式表示
//! 不支持** —— 而不是猜一个键名，把接管做成「看着开着、其实空转」。

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

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
    /// 国际版取的是 CLI `gtn()` 的 `prod` 分支（`api2-v2`，与桌面端 asar 里那个
    /// `api2.qoder.sh` 不是同一个 —— 实测能用的是前者，接管线一直用它）；
    /// 国内版两边都是 `gateway.qoder.com.cn`。
    ///
    /// 这里曾经另有一个 `catalog_base()`（国际版写成 `api3.qoder.sh`）—— 那是早先
    /// 一次没验证过的猜测。国内版实测目录就在本函数的域上（`models` 模块头），
    /// 国际版走同一条代码路径；留两个常量只会让「哪一天对不上」变成两个答案。
    pub fn infer_base(self) -> &'static str {
        match self {
            Region::Global => "https://api2-v2.qoder.sh",
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

    /// 智能接管要写进客户端的**端点环境变量键**；该区域不支持时返回 None。
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
    /// # 两个区域的开关是相反的，所以这里必须分叉
    ///
    /// `v7a()` 开头那句 `if(!Ja) return` 说明**它只在一个区域的构建里生效**：
    /// `Ja` 在国内版里恒真、国际版里恒假 —— 国际版读的是 `U7a()`
    /// （`QODER_CENTER_ENDPOINT`），而它**只覆盖 center**：推理端点还得靠代答
    /// `/api/v3/service/region/endpoints` 的选举结果。
    ///
    /// 于是国际版这里返回 `None`，让上层明确报「尚未支持」——
    /// 而不是写一段**看起来生效、实际没人读**的注入。那正是这块历史踩过的坑：
    /// 配置写成功、界面显示已开启、端口在听，而对话一直直连官方。
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
    pub fn endpoint_env_key(self) -> Option<&'static str> {
        match self {
            Region::Cn => Some("QODERCN_SERVER_ENDPOINT"),
            Region::Global => None,
        }
    }

    /// macOS 应用包路径。
    pub fn macos_app_dir(self) -> &'static str {
        match self {
            Region::Global => "/Applications/Qoder.app",
            Region::Cn => "/Applications/Qoder CN.app",
        }
    }

    /// 智能接管的落点：官方 agent SDK 在 `app.asar.unpacked` 里的根目录。
    ///
    /// # 为什么是 asar 外这份
    ///
    /// 客户端起推理进程前会把路径里的 `app.asar` 换成 `app.asar.unpacked`
    /// （产物里的 `xt()`），命中就直接用，并留下诊断
    /// `[WorkerTransport] Using asar-unpacked worker runtime: …`。
    /// 也就是说**被执行的正是 asar 之外那一份** —— 它不在归档完整性校验范围内，
    /// 可以直接改；而 asar 内那份永远轮不到。
    ///
    /// 具体文件由 [`crate::patch`] 按与客户端一致的顺序探测。
    pub fn worker_sdk_root(self) -> Option<std::path::PathBuf> {
        let sdk = match self {
            Region::Global => "@qoder-ai/qoder-agent-sdk",
            Region::Cn => "@qoder-ai/qoder-cn-agent-sdk",
        };
        Some(
            std::path::Path::new(self.macos_app_dir())
                .join("Contents/Resources/app.asar.unpacked/node_modules")
                .join(sdk),
        )
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

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
        assert_eq!(Region::Global.infer_base(), "https://api2-v2.qoder.sh");
        // 国际版的推理网关（模型目录也在它上面）与 openapi 不是同一个域，别顺手统一
        assert_ne!(Region::Global.infer_base(), Region::Global.openapi_base());
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

    /// 接管的端点键**只有国内版有**：`v7a()` 的 `if(!Ja) return` 让它在国际版构建里
    /// 恒不生效，国际版覆盖的是别的键、且只覆盖 center。所以国际版必须返回 None
    /// —— 上层据此明确报「尚未支持」，而不是写一段没人读的注入。
    #[test]
    fn endpoint_env_key_exists_only_where_it_is_actually_read() {
        assert_eq!(Region::Cn.endpoint_env_key(), Some("QODERCN_SERVER_ENDPOINT"));
        assert_eq!(Region::Global.endpoint_env_key(), None);
        // 键名前缀与区域一一对应，别把两边的键搞混（读错区域 = 覆盖静默失效）
        for (r, key) in [
            (Region::Cn, Region::Cn.endpoint_env_key().unwrap()),
            (Region::Global, "QODER_CENTER_ENDPOINT"),
        ] {
            let want = if r == Region::Cn { "QODERCN_" } else { "QODER_" };
            assert!(key.starts_with(want), "{key} 的前缀应与 {} 对应", r.label());
        }
    }

    /// 接管落点必须指向 asar **之外**那份被执行的产物，并且两个客户端各指各的 SDK。
    #[test]
    fn takeover_target_points_at_the_unpacked_sdk_of_each_client() {
        let cn = Region::Cn.worker_sdk_root().unwrap();
        assert_eq!(
            cn.to_string_lossy(),
            "/Applications/Qoder CN.app/Contents/Resources/app.asar.unpacked/node_modules/@qoder-ai/qoder-cn-agent-sdk"
        );
        let g = Region::Global.worker_sdk_root().unwrap();
        assert_eq!(
            g.to_string_lossy(),
            "/Applications/Qoder.app/Contents/Resources/app.asar.unpacked/node_modules/@qoder-ai/qoder-agent-sdk"
        );
        // 关键不变量：路径里必须是 `app.asar.unpacked`，不能落在 `app.asar` 内
        // （asar 内那份不受我们控制，也不会被执行）
        for p in [cn, g] {
            let s = p.to_string_lossy().to_string();
            assert!(s.contains("app.asar.unpacked"), "{s}");
            assert!(!s.contains("app.asar/node_modules"), "{s}");
        }
    }
}
