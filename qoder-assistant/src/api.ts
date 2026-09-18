import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  Settings,
  LocalAccount,
  OAuthStart,
  OAuthPoll,
  CheckinLog,
  DayEntry,
  ImportItem,
  ImportReport,
  BrokerStatus,
  PoolOp,
  NetReport,
  NetRestoreReport,
  StealthStatus,
  JournalEvent,
  ModelReport,
} from "./types";

export const listAccounts = () => invoke<Account[]>("list_accounts");

/**
 * 批量导入账号（「导入本机账号」与「登录新账号」共用）。
 * 已存在的账号（手机号或 token 相同）会被合并补全凭证，不会产生重复条目。
 */
export const importAccounts = (items: ImportItem[]) =>
  invoke<ImportReport>("import_accounts", { items });

export const removeAccount = (id: string) =>
  invoke<void>("remove_account", { id });

export const checkinOne = (id: string) =>
  invoke<Account>("checkin_one", { id });

export const checkinAll = () => invoke<Account[]>("checkin_all");

/** 一键刷新：不打签到接口，重拉并持久化全部账号的积分快照 / 签到状态 / 积分余量 */
export const refreshAll = () => invoke<Account[]>("refresh_all");

/** 首选通道：读本机 Qoder 登录文件（auth/*.info），含昵称与手机号 */
export const discoverLocalAccounts = () =>
  invoke<LocalAccount[]>("discover_local_accounts");

/**
 * 「无感登录」第一步：申请一次设备授权会话，拿到授权链接。
 *
 * **不收域参数**：Qoder 只有一套 Global 域，地址由后端 `qoder_api` 唯一决定。
 */
export const oauthStart = () => invoke<OAuthStart>("oauth_start");

/** 「无感登录」第二步：轮询授权结果；done=false 表示仍需继续轮询 */
export const oauthPoll = (loginId: string) =>
  invoke<OAuthPoll>("oauth_poll", { loginId });

/** 在系统默认浏览器打开链接（授权页） */
export const openExternal = (url: string) =>
  invoke<void>("open_external", { url });

// ── 凭证池：四个命令都是**池级**的，都不带账号 id ──────────────────────────
//
// 一池一个 uuid、池内一把闸，所以这里的动作作用范围是「整台机器」。
// 续签仍在本机执行：管家只负责存取与发闸（见 src-tauri/src/broker.rs）。

/** 把本机这一批账号整体上传，管家颁发 uuid 并当场绑定 */
export const brokerUpload = () => invoke<PoolOp>("broker_upload");

/** 绑定别处复制过来的 uuid（成功后立刻整池同步一轮，把本地独有的账号也推上去） */
export const brokerLink = (uuid: string) => invoke<PoolOp>("broker_link", { uuid });

/** 解绑：摘掉本地 uuid（云端那一池保留），并移除本机与云端重复的凭证 */
export const brokerUnbind = () => invoke<BrokerStatus>("broker_unbind");

/** 只读状态（绑没绑 / uuid / 版本 / 上次同步）。无副作用，可随时轮询 */
export const brokerState = () => invoke<BrokerStatus>("broker_state");

export const getSettings = () => invoke<Settings>("get_settings");

export const saveSettings = (settings: Settings) =>
  invoke<Settings>("save_settings", { settings });

/** 原子应用设置；接管启停或换端口时会安全重启 Qoder 与长驻 CLI host */
export const applySettings = (settings: Settings) =>
  invoke<Settings>("apply_settings", { settings });

/** 发一条测试通知，返回推送服务的原始响应 */
export const testNotify = (webhook: string) =>
  invoke<string>("test_notify", { webhook });

/** 是否已注册开机自启动（以操作系统为准） */
export const getAutostart = () => invoke<boolean>("get_autostart");

/** 开启/关闭开机自启动，返回落定后的真实状态 */
export const setAutostart = (enabled: boolean) =>
  invoke<boolean>("set_autostart", { enabled });

export const appVersion = () => invoke<string>("app_version");

export const getCheckinLogs = (limit?: number, accountId?: string) =>
  invoke<CheckinLog[]>("get_checkin_logs", {
    limit: limit ?? null,
    accountId: accountId ?? null,
  });

/** 清空签到日志：不传 accountId 则清空全部 */
export const clearCheckinLogs = (accountId?: string) =>
  invoke<void>("clear_checkin_logs", { accountId: accountId ?? null });

/**
 * 诊断 Qoder 的全局网络配置（**只读**，不会改动任何文件）。
 * 扫的是：`~/.qoder/settings.json` / `~/.codebuddy/settings.json` 里的
 * `endpoint` 与 `env.CODEBUDDY_*`、launchd 全局环境变量、shell 启动脚本、本应用反代开关。
 */
export const netDiagnose = () => invoke<NetReport>("net_diagnose");

/**
 * 一键恢复：备份 → 清除调试残留键 → 取消 launchd 全局变量 → 关闭本地反代 → 复检。
 * 只碰「明确是调试写进去」的键，不认识的一律原样保留。
 */
export const netRestore = () => invoke<NetRestoreReport>("net_restore");

/** 在系统文件管理器里定位某个文件（用于查看备份） */
export const revealPath = (path: string) => invoke<void>("reveal_path", { path });

/** 查询智能接管状态（只读） */
export const stealthStatus = () => invoke<StealthStatus>("stealth_status");

/** 接管事件流（新的在前）：开启 / 关闭 / 开始使用账号 / 重启 / 错误 */
export const takeoverEvents = () => invoke<JournalEvent[]>("takeover_events");
/** 清空接管动态（不可恢复） */
export const clearTakeoverEvents = () => invoke<void>("takeover_events_clear");

/**
 * 「限流切换」的模型清单（三层来源：Qoder 官方目录 / 落盘快照 / 本机痕迹）。
 *
 * refresh=true 时忽略内存缓存强制重拉——但仍会依次退到后两层，
 * 所以点一次「刷新」不会把界面刷成空的。
 */
export const freeModels = (refresh: boolean) =>
  invoke<ModelReport>("free_models", { refresh });

/**
 * 积分简报的**日条目**（新的在前）。
 *
 * 日条目是当天时条目之和、由后端现算（不落盘），所以读到的永远和展开看到的一致。
 * 条目只由后台每小时结算产生，**没有任何手动生成的入口**。
 */
export const creditBriefing = () => invoke<DayEntry[]>("credit_briefing");

/** 清空简报历史（时条目 + 台账里的小时桶；逐包累计值保留） */
export const clearCreditBriefing = () =>
  invoke<void>("credit_briefing_clear");

/**
 * **开启积分简报**：清历史 → 立刻采一次样 → 只对齐基线（不记这一段增量）。
 *
 * 断档期（应用没开着的那几天）攒下的增量既归不到具体的小时、又不该算进开启后的
 * 第一个小时，所以开启时宁可不记，也不让用户一开启就看到一笔巨额消耗。
 */
export const enableCreditBriefing = () =>
  invoke<void>("credit_briefing_enable");

/** 后台每小时固化出新的时条目时后端发的通知（payload: { hours: number }） */
export const BRIEFING_SEALED_EVENT = "credit-briefing-sealed";

/**
 * 采集到新积分事实时后端发的广播（payload: `CreditRow[]`）。
 *
 * **所有采集路径都会发**：每小时采样 / 手动刷新 / 签到 / 开启简报 / 接管路由补拉。
 * 前端在 `src/credits.ts` 里统一接上，并进那份全局内存对象 ——
 * 账号页与简报页读的是它，所以一次采集会让两个页面同时更新。
 */
export const CREDITS_UPDATED_EVENT = "credits-updated";
