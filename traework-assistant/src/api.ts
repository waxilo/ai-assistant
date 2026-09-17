import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  NewAccount,
  Settings,
  CheckinResult,
  LogEntry,
  JournalEvent,
  AcctStatus,
  OAuthStart,
  OAuthPoll,
  TakeoverStatus,
  TakeoverRules,
  BrokerStatus,
  PoolOp,
  DayEntry,
} from "./types";

export const listAccounts = () => invoke<Account[]>("list_accounts");
/** 按需回源补全账号资料（占位名/缺失手机号 → 服务端真实昵称与脱敏手机号），返回更新后的列表 */
export const refreshAccountProfiles = () => invoke<Account[]>("refresh_account_profiles");
/** 导入账号：`id` / `created_at` 交给后端补（见 `NewAccount`） */
export const importAccounts = (accounts: NewAccount[]) =>
  invoke<Account[]>("import_accounts", { accounts });
export const removeAccount = (id: string) =>
  invoke<Account[]>("remove_account", { id });
export const discoverLocal = () => invoke<Account[]>("discover_local");
export const checkinOne = (id: string) => invoke<CheckinResult>("checkin_one", { id });
export const checkinAll = () => invoke<CheckinResult[]>("checkin_all");
/** 每个账号的签到状态 + **账号已有积分**（后端直接返回结构化数据，前端不再自行解析 JSON） */
export const checkinStatus = () => invoke<AcctStatus[]>("checkin_status");
export const getSettings = () => invoke<Settings>("get_settings");
export const saveSettings = (settings: Settings) =>
  invoke<Settings>("save_settings", { settings });
export const getLogs = () => invoke<LogEntry[]>("get_logs");
export const clearLogs = () => invoke<void>("clear_logs");

/** 上传：创建 / 复用本机凭证池，返回 uuid —— 唯一要展示给用户复制的东西 */
export const brokerUpload = () => invoke<PoolOp>("broker_upload");
/** 绑定别处复制过来的 uuid（成功后立刻整池同步一轮，把本地独有的账号也推上去） */
export const brokerLink = (uuid: string) => invoke<PoolOp>("broker_link", { uuid });
/** 解绑：摘掉本地 uuid，删掉本机「与云端一致」的账号（云端那一池保留） */
export const brokerUnbind = () => invoke<BrokerStatus>("broker_unbind");
/** 只读状态（绑没绑 / uuid / 版本 / 上次同步）。无副作用，可随时轮询 */
export const brokerState = () => invoke<BrokerStatus>("broker_state");
export const oauthStart = (host?: string | null) =>
  invoke<OAuthStart>("oauth_start", { host: host ?? null });
export const oauthPoll = (loginId: string) =>
  invoke<OAuthPoll>("oauth_poll", { loginId });
export const openExternal = (url: string) => invoke<void>("open_external", { url });

export const getTakeoverStatus = () => invoke<TakeoverStatus>("takeover_status");
export const enableTakeover = () => invoke<TakeoverStatus>("takeover_enable");
export const disableTakeover = () => invoke<TakeoverStatus>("takeover_disable");
/**
 * 改「接管哪些应用」——**开关开着时也能改**（命令存在的全部理由）。
 *
 * `ids` 语义与界面一致：**空数组 = 全部**（与「参与扣费的账号」同一套），非空 = 只接管这些。
 * 已开启时后端做**增量协调**：新勾上的补丁 + 改道并重启它，取消的还原并重启它，
 * 没变的应用一个字节都不碰；未开启时只记设置。
 */
export const setTakeoverApps = (ids: string[]) =>
  invoke<TakeoverStatus>("takeover_set_apps", { ids });
/** 接管动态（最新在前）：谁用了哪个账号、有没有限流换号、代理是否报错 */
export const takeoverEvents = () => invoke<JournalEvent[]>("takeover_events");
export const clearTakeoverEvents = () => invoke<void>("clear_takeover_events");
// 「打 / 还原 TraeWork 补丁」这两个动作**没有独立命令**：补丁的生命周期已经并进
// `takeover_enable` / `takeover_disable`（开接管自动打、关接管自动还原），
// 界面因此不需要、也不应该再单独碰它 —— 见 `src-tauri/src/commands.rs` 的补丁小节。
/** 当前接管规则（`proxy-rules.json`）。写入后**立即生效**（读侧只有 1 秒缓存）。 */
export const getTakeoverRules = () => invoke<TakeoverRules>("takeover_rules");
export const saveTakeoverRules = (rules: TakeoverRules) =>
  invoke<TakeoverRules>("takeover_save_rules", { rules });

// ── 积分简报 ─────────────────────────────────────────────────

/**
 * 读积分简报的**日条目**（后端现算：当天时条目之和，不落盘），新的在前。
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
