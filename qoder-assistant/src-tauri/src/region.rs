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
//! | 模型网关（接管转发目标） | `https://api2-v2.qoder.sh` | `https://gateway.qoder.com.cn` |
//! | 模型目录（`/api/v2/model/list`） | `https://api3.qoder.sh` | `https://gateway.qoder.com.cn` |
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
//! 但**接管用的那个环境变量键名在 CN 侧没有实测依据**（CLI 是独立二进制，
//! 键名由 `${prefix}_...` 拼出来）。所以 [`Region::takeover_env_key`] 两个区域都返回
//! 同一个已实测可用的键，并在该处写明了这条不确定性 —— 与其猜一个新键把现网可用的
//! 接管弄坏，不如让两边共用同一个已验证的键。

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
    /// 全部区域（界面上的顺序就是它）
    pub const ALL: [Region; 2] = [Region::Global, Region::Cn];

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

    /// 模型网关：接管反代把 CLI 的对话请求转发到它。
    ///
    /// 国际版取的是 CLI `gtn()` 的 `prod` 分支（`api2-v2`，与桌面端 asar 里那个
    /// `api2.qoder.sh` 不是同一个 —— 实测能用的是前者，接管线一直用它）；
    /// 国内版两边都是 `gateway.qoder.com.cn`。
    pub fn infer_base(self) -> &'static str {
        match self {
            Region::Global => "https://api2-v2.qoder.sh",
            Region::Cn => "https://gateway.qoder.com.cn",
        }
    }

    /// 模型目录基址（`GET {catalog_base}/api/v2/model/list`）。
    ///
    /// 两个区域**不是同一个域**：国际版在 `api3.qoder.sh`，国内版在
    /// `gateway.qoder.com.cn`（实测日志见模块头）。
    pub fn catalog_base(self) -> &'static str {
        match self {
            Region::Global => "https://api3.qoder.sh",
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

    // ---------------------------------------------------------------- 官方客户端

    /// 官方客户端的可执行名 / `open -a` 用的名字（`CFBundleExecutable`，也是
    /// `open -a` 认的那个名字）。
    pub fn app_name(self) -> &'static str {
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

    /// macOS 主可执行文件的绝对路径。
    ///
    /// ⚠️ 上一版这里的正则写的是 `.../MacOS/Electron`，**一个进程都匹配不到**：
    /// Qoder 的 `CFBundleExecutable` 是 `Qoder`（国内版是 `Qoder CN`），
    /// `Contents/MacOS/` 下也没有叫 `Electron` 的文件（只有
    /// `Frameworks/Electron Framework.framework`）。所以退出/重启/判断是否在跑
    /// 这三件事一直是「假装做完了」。这里按 `Info.plist` 的真值拼。
    pub fn macos_exec_path(self) -> String {
        format!("{}/Contents/MacOS/{}", self.macos_app_dir(), self.app_name())
    }

    /// 匹配「官方客户端本体 + 它的全部子进程」的正则（macOS `pkill -f` / `kinfo_proc`）。
    ///
    /// `($| )` 让 `.../MacOS/Qoder --type=renderer` 这类带参数的子进程一起命中。
    pub fn macos_process_pattern(self) -> String {
        format!("^{}($| )", regex_escape(&self.macos_exec_path()))
    }

    /// 官方客户端里**长驻 CLI host** 的进程正则。
    ///
    /// 它由客户端从自己的解包资源里拉起（旧版本是 `app.asar.unpacked/cli/`，
    /// 0.3.3 已经改成 `app.asar.unpacked/node_modules/@qoder-ai/*`）。
    /// 因此这里只钉「必须属于这个 app 的解包资源根」这一条**稳定不变量**，
    /// 不再跟随官方内部结构调整的具体子目录 —— 上一版写死 `cli/` 的下场就是
    /// 那条正则指向一个并不存在的目录。
    pub fn cli_host_process_pattern(self) -> String {
        format!(
            "{}/Contents/Resources/app\\.asar\\.unpacked/",
            regex_escape(self.macos_app_dir())
        )
    }

    /// 接管写进 CLI 配置的**环境变量键**。
    ///
    /// 两个区域目前共用同一个键：`CODEBUDDY_BASE_URL` 是 `~/.qoder/settings.json` 里
    /// 那个已被现网验证过的键（`netfix` 的 `ENV_KEYS` 与它一致），而国内版的
    /// `cliEnvironmentPrefix` 虽然叫 `QODERCN`，**CLI 侧真正的键名没有实测依据**
    /// （CLI 是独立二进制，`${prefix}_...` 只是拼法猜测）。猜一个新键的代价是把
    /// 现在能用的接管弄坏，收益是零 —— 所以先共用，等真机验证后再分叉。
    pub fn takeover_env_key(self) -> &'static str {
        let _ = self;
        "CODEBUDDY_BASE_URL"
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// `regex` 语法里的元字符转义（我们只用它来处理路径，而路径里有 `Qoder CN.app` 的
/// 空格与 `.`；空格不需要转义，`.` 需要）。
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
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
        assert_eq!(Region::Cn.catalog_base(), "https://gateway.qoder.com.cn");
        // 模型目录的国际版不在 openapi 上（在 api3），别顺手统一
        assert_eq!(Region::Global.catalog_base(), "https://api3.qoder.sh");
        assert_ne!(Region::Global.catalog_base(), Region::Global.openapi_base());
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

    /// 两个官方客户端是**两个 app**：路径、可执行名、进程正则都不能相同。
    /// 特别是正则必须匹配 `CFBundleExecutable` 的真值（`Qoder` / `Qoder CN`），
    /// 而不是历史上写错的 `Electron`。
    #[test]
    fn process_patterns_point_at_the_real_executables() {
        assert_eq!(
            Region::Global.macos_exec_path(),
            "/Applications/Qoder.app/Contents/MacOS/Qoder"
        );
        assert_eq!(
            Region::Cn.macos_exec_path(),
            "/Applications/Qoder CN.app/Contents/MacOS/Qoder CN"
        );
        assert!(!Region::Global.macos_process_pattern().contains("Electron"));
        // 路径里的 `.` 要转义，空格不用
        assert_eq!(
            Region::Global.macos_process_pattern(),
            "^/Applications/Qoder\\.app/Contents/MacOS/Qoder($| )"
        );
        assert_eq!(
            Region::Cn.macos_process_pattern(),
            "^/Applications/Qoder CN\\.app/Contents/MacOS/Qoder CN($| )"
        );
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
        // 回归护栏：这里**曾经**按 `<app_name> Safe Storage` 拼（少一个 `App`），
        // 实测 `Qoder Safe Storage` 根本不存在。这条断言让那种"顺手统一"改法当场失败。
        for region in Region::ALL {
            assert_ne!(
                region.keychain_service(),
                format!("{} Safe Storage", region.app_name()),
                "钥匙串名不是由 app_name 拼出来的（{}）",
                region.label()
            );
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
}
