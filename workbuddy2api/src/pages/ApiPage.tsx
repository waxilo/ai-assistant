import { useCallback, useEffect, useMemo, useState } from "react";
import type { Account, ChatModel, CodexState, GatewayState, Settings } from "../types";
import {
  applySettings,
  chatModels,
  codexAppRunning,
  codexConnect,
  codexDisconnect,
  codexRestart,
  codexStatus,
  gatewayKeyRotate,
  gatewayStatus,
} from "../api";
import { accountLabel, copyText } from "../common";
import type { ConfirmReq, Toast } from "../common";
import { creditsOf, expiryOf, useCredits } from "../credits";
import {
  IconAlertTriangle,
  IconBolt,
  IconChevron,
  IconList,
  IconPlug,
  IconPower,
  IconRefresh,
  IconShield,
  IconUsers,
} from "../components/Icons";
import { Row } from "../components/SettingsControls";
import Switch from "../components/Switch";
import { Dialog } from "../components/Dialog";

interface Props {
  settings: Settings;
  accounts: Account[];
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  onSettings: (s: Settings) => void;
  onToast: (t: Toast) => void;
}

/** Key 打码：只露前 8 后 4，足够辨认是哪一枚，又不足以被截屏带走 */
function maskKey(k: string): string {
  if (!k) return "（尚未生成）";
  if (k.length <= 14) return k;
  return `${k.slice(0, 8)}…${k.slice(-4)}`;
}

/** 全选归一：勾满全部账号时存空（= 默认全选，新增账号自动可扣费） */
function normalizeBilling(list: string[], allIds: string[]): string[] {
  return allIds.length > 0 && list.length === allIds.length ? [] : list;
}

/** 到期日的短格式：`11-30`。未知返回空串，界面就不显示这一段 */
function expiryDay(ms: number | null): string {
  if (ms == null) return "";
  const d = new Date(ms);
  if (isNaN(d.getTime())) return "";
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return `${mm}-${dd}`;
}

export function ApiPage({ settings, accounts, askConfirm, onSettings, onToast }: Props) {
  const book = useCredits();
  const [state, setState] = useState<GatewayState | null>(null);
  const [models, setModels] = useState<ChatModel[]>([]);
  const [modelErr, setModelErr] = useState("");
  const [modelBusy, setModelBusy] = useState(false);
  const [modelQuery, setModelQuery] = useState("");
  // 可用模型清单：只读清单也走弹窗（与「参与扣费」同一套入口样式），
  // 不再把几十行模型直接铺在「接入 Codex」卡片里
  const [modelsOpen, setModelsOpen] = useState(false);

  // 扣费备选账号：同样是草稿制弹窗（与「参与扣费」同一套交互）
  const [billOpen, setBillOpen] = useState(false);
  const [billDraft, setBillDraft] = useState<string[]>([]);
  const [billQuery, setBillQuery] = useState("");

  // Codex 接入：状态是**文件里的事实**（后端回读 config.toml），不是本地记忆
  const [codex, setCodex] = useState<CodexState | null>(null);
  const [codexOpen, setCodexOpen] = useState(false);
  const [codexDraft, setCodexDraft] = useState<string[]>([]);
  const [codexDefault, setCodexDefault] = useState("");
  const [codexQuery, setCodexQuery] = useState("");
  const [codexBusy, setCodexBusy] = useState(false);
  /** Codex 应用此刻在不在跑：在跑才谈得上「要重启才生效」 */
  const [codexRunning, setCodexRunning] = useState(false);

  const [showKey, setShowKey] = useState(false);
  const [portDraft, setPortDraft] = useState(String(settings.gateway_port));
  const [busy, setBusy] = useState(false);

  // 监听状态是**进程实时事实**（不是配置）：轮询比读设置更可信——
  // 端口被占用时开关照样是「已开启」，只有这里能看出它其实没在监听。
  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const s = await gatewayStatus();
        if (alive) setState(s);
      } catch {
        /* 命令不可用（旧版本）时保持上一次状态 */
      }
    };
    void tick();
    const t = window.setInterval(tick, 2000);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, []);

  useEffect(() => {
    setPortDraft(String(settings.gateway_port));
  }, [settings.gateway_port]);

  /**
   * 拉上游可用的对话模型清单。
   *
   * 一份清单两处用：Codex 接入弹窗的候选集，以及手动填别的客户端时照着抄。
   * 拉不到不弹窗（进页面就失败太吵），把原因写进 `modelErr` 显示在卡片里。
   */
  const refreshModels = useCallback(async () => {
    setModelBusy(true);
    try {
      const list = await chatModels();
      setModels(list);
      setModelErr("");
    } catch (e) {
      setModels([]);
      setModelErr(String(e));
    } finally {
      setModelBusy(false);
    }
  }, []);

  useEffect(() => {
    void refreshModels();
  }, [refreshModels]);

  // 刚打开网关时模型清单可能还没拉到（后端要现打一次上游）：开关一变就补拉一次。
  // 幂等——拉到过的东西会走 1h 缓存，不会每个开关动作都打上游。
  useEffect(() => {
    if (settings.gateway_enabled) void refreshModels();
  }, [settings.gateway_enabled, refreshModels]);

  const patch = useCallback(
    async (p: Partial<Settings>) => {
      const next = { ...settings, ...p };
      try {
        const saved = await applySettings(next);
        onSettings(saved);
        return saved;
      } catch (e) {
        onToast({ kind: "err", text: String(e) });
        return null;
      }
    },
    [settings, onSettings, onToast],
  );

  const toggle = async (on: boolean) => {
    if (on) {
      if (accounts.length === 0) {
        onToast({ kind: "err", text: "还没有账号：先导入账号再开网关" });
        return;
      }
      const ok = await askConfirm({
        title: "开启对外 API 网关",
        body:
          `开启后会在本机 127.0.0.1:${settings.gateway_port} 监听（只对本机开放），` +
          "任何持有 API Key 的客户端都能消耗你账号池的积分。\n\n" +
          "请确认：这属于把桌面端额度当 API 使用，请求特征与官方客户端差异很大，存在被风控或封号的风险。",
        okText: "确认开启",
        danger: true,
      });
      if (!ok) return;
    }
    setBusy(true);
    await patch({ gateway_enabled: on });
    setBusy(false);
  };

  const rotate = async () => {
    const ok = await askConfirm({
      title: "重新生成 API Key",
      body: "旧 Key 会立即失效，所有已配置的客户端都需要换成新 Key。账号凭证不受影响。",
      okText: "重新生成",
    });
    if (!ok) return;
    setBusy(true);
    try {
      const key = await gatewayKeyRotate();
      onSettings({ ...settings, gateway_api_key: key });
      onToast({ kind: "ok", text: "已生成新 Key，记得更新客户端配置" });
    } catch (e) {
      onToast({ kind: "err", text: String(e) });
    } finally {
      setBusy(false);
    }
  };

  const commitPort = async () => {
    const n = Number(portDraft);
    if (!Number.isInteger(n) || n < 1024 || n > 65535) {
      onToast({ kind: "err", text: "端口需在 1024–65535 之间" });
      setPortDraft(String(settings.gateway_port));
      return;
    }
    if (n === settings.gateway_port) return;
    await patch({ gateway_port: n });
  };

  const copy = async (text: string, label: string) => {
    const ok = await copyText(text);
    onToast({ kind: ok ? "ok" : "err", text: ok ? `${label}已复制` : "复制失败，请手动选中" });
  };

  const openModels = () => {
    setModelQuery("");
    setModelsOpen(true);
    // 清单为空（进页面时网关还没开、或上次拉失败）就顺手补拉一次；
    // 拉过的走 1h 缓存，不会每次点开都打上游
    if (models.length === 0 && !modelBusy) void refreshModels();
  };

  const openBillPick = () => {
    // 空 = 全部参与：弹窗里就**全部勾上**，否则「全部账号（N）」的入口点进去一片空白
    setBillDraft(billingSet ? [...billingSet] : [...allIds]);
    setBillQuery("");
    setBillOpen(true);
  };

  const saveBillPick = async () => {
    setBillOpen(false);
    await patch({ billing_account_ids: normalizeBilling(billDraft, allIds) });
  };

  // ── Codex 接入 ────────────────────────────────────────────────
  //
  // 这一组是全页唯一会**改写别的应用配置文件**的动作（`~/.codex/config.toml`），
  // 所以每次动作之后都回读一次后端状态 —— 写成功不等于写对了。

  const refreshCodex = useCallback(async () => {
    try {
      setCodex(await codexStatus());
    } catch {
      setCodex(null); // 后端不支持时宁可显示「未知」，不编造状态
    }
    setCodexRunning(await codexAppRunning().catch(() => false));
  }, []);

  useEffect(() => {
    void refreshCodex();
  }, [refreshCodex]);

  const openCodexPick = () => {
    const known = new Set(models.map((m) => m.slug));
    // 已接入过就按**目录文件里的**模型回填（用户手改过也对得上）；没接过则预选第一个
    const seed = (codex?.catalogModels ?? []).filter((s) => known.has(s));
    const initial = seed.length > 0 ? seed : models.slice(0, 1).map((m) => m.slug);
    setCodexDraft(initial);
    setCodexDefault(
      codex?.model && initial.includes(codex.model) ? codex.model : (initial[0] ?? ""),
    );
    setCodexQuery("");
    setCodexOpen(true);
  };

  const doCodexConnect = async () => {
    if (codexDraft.length === 0) {
      onToast({ kind: "err", text: "至少要勾选一个模型：目录是空的，Codex 里就一个都选不出来" });
      return;
    }
    setCodexBusy(true);
    try {
      const st = await codexConnect(codexDraft, codexDefault || codexDraft[0]);
      setCodex(st);
      setCodexOpen(false);
      const running = await codexAppRunning().catch(() => false);
      setCodexRunning(running);
      onToast({
        kind: "ok",
        text: running
          ? "已接入 Codex：它正在运行，点「重启 Codex」之后才生效"
          : "已接入 Codex：下次启动 Codex 时生效",
      });
    } catch (e) {
      onToast({ kind: "err", text: String(e) });
    } finally {
      setCodexBusy(false);
    }
  };

  const doCodexDisconnect = async () => {
    const ok = await askConfirm({
      title: "断开 Codex 接入",
      body:
        "会从 ~/.codex/config.toml 里删掉本网关的 provider 段，并把改过的键恢复成接入前的原值" +
        `（原件备份在应用数据目录）。${
          codexRunning ? "Codex 正在运行，断开后要重启它才会回到原来的配置。" : ""
        }`,
      okText: "断开",
      danger: true,
    });
    if (!ok) return;
    setCodexBusy(true);
    try {
      const st = await codexDisconnect();
      setCodex(st);
      onToast({ kind: "ok", text: "已断开：Codex 配置已还原" });
    } catch (e) {
      onToast({ kind: "err", text: String(e) });
    } finally {
      setCodexBusy(false);
    }
  };

  const doCodexRestart = async () => {
    setCodexBusy(true);
    try {
      const r = await codexRestart();
      await refreshCodex();
      onToast({
        kind: r.restarted ? "ok" : "info",
        text: r.restarted
          ? `已重启 ${r.app ?? "Codex"}，新配置生效`
          : "Codex 当时没在运行，不用重启（下次启动会读到新配置）",
      });
    } catch (e) {
      onToast({ kind: "err", text: String(e) });
    } finally {
      setCodexBusy(false);
    }
  };

  // 只绑回环，所以这里不需要再"按配置拼 host"——写死 127.0.0.1 与网关一致
  const baseUrl = `http://127.0.0.1:${settings.gateway_port}/v1`;
  const listening = state?.listening ?? false;
  const allIds = useMemo(() => accounts.map((a) => a.id), [accounts]);
  const billing = settings.billing_account_ids;
  const billingSet = billing.length === 0 ? null : new Set(billing);
  const billCount = billingSet ? billingSet.size : accounts.length;

  const modelFiltered = models.filter((m) => {
    const q = modelQuery.trim().toLowerCase();
    return !q || m.slug.toLowerCase().includes(q) || m.name.toLowerCase().includes(q);
  });
  const billFiltered = accounts.filter((a) => {
    const q = billQuery.trim().toLowerCase();
    return !q || `${a.name} ${a.phone ?? ""}`.toLowerCase().includes(q);
  });
  const codexFiltered = models.filter((m) => {
    const q = codexQuery.trim().toLowerCase();
    return !q || m.slug.toLowerCase().includes(q) || m.name.toLowerCase().includes(q);
  });
  /** 弹窗里「默认模型」下拉的候选：只列已勾上的（默认模型必须写进同一份目录） */
  const codexPicked = models.filter((m) => codexDraft.includes(m.slug));

  return (
    <div className="panel-page">
      <div className="hero-block">
        <div className="hero-top">
          <div className="hero-copy">
            <span className="hero-icon">
              <IconBolt size={19} />
            </span>
            <div className="hero-text">
              <div className="hero-title">
                <h2>对外 API 网关</h2>
              </div>
            </div>
          </div>
          <Switch
            checked={settings.gateway_enabled}
            disabled={busy}
            size="lg"
            title="开启后会在本机监听一个端口，供客户端用 API Key 调用"
            onChange={(v) => void toggle(v)}
          />
        </div>
        <div className="hero-desc">
          把账号池的积分变成一个 OpenAI 兼容接口：只在本机起端口、每个请求都要带 Key、按积分到期时间自动选号。
          这条通道<b>不碰官方客户端</b>的任何配置，客户端该登录还登录、该用还照用。
        </div>
      </div>

      {settings.gateway_enabled && (
        <div className="card">
          <div className="card-head">
            <span>连接信息</span>
            {/* badge 的修饰类是 badge-ok / badge-idle / badge-already（连字符），
                不是 badge.ok —— 写错就是「灰底灰字」，看起来像没样式 */}
            <span className={`badge ${listening ? "badge-ok" : "badge-already"}`}>
              {listening ? `监听中 · ${state?.port ?? settings.gateway_port}` : "未监听"}
            </span>
          </div>

          {state?.error && (
            <div className="note danger">
              <IconAlertTriangle />
              <span>
                {state.error}
                <div style={{ opacity: 0.75, marginTop: 6 }}>
                  端口被别的程序占用时就是这个提示。换一个端口，或先关掉占用它的程序。
                </div>
              </span>
            </div>
          )}

          <div className="kv-row">
            <span className="field-label">Base URL</span>
            <code className="mono kv-value">{baseUrl}</code>
            <button className="btn small" onClick={() => void copy(baseUrl, "Base URL")}>
              复制
            </button>
          </div>

          <div className="kv-row">
            <span className="field-label">API Key</span>
            <code className="mono kv-value">
              {showKey ? settings.gateway_api_key || "（尚未生成）" : maskKey(settings.gateway_api_key)}
            </code>
            <button className="btn small" onClick={() => setShowKey((v) => !v)}>
              {showKey ? "隐藏" : "显示"}
            </button>
            <button
              className="btn small"
              onClick={() => void copy(settings.gateway_api_key, "API Key")}
              disabled={!settings.gateway_api_key}
            >
              复制
            </button>
            <button className="btn small" onClick={() => void rotate()} disabled={busy}>
              <IconRefresh /> 重新生成
            </button>
          </div>

          <div className="note warn">
            <IconShield />
            <span>
              Key 等同于账号池的使用权。不要提交进仓库、不要发到群里；
              怀疑泄露就点「重新生成」——它只换密钥，不影响任何账号的登录状态。
            </span>
          </div>
        </div>
      )}

      <div className="card">
        <div className="card-head">网关</div>
        <div className="set-group">
          <Row
            title="端口"
            desc="默认 8790。改完 500ms 内自动重绑，不用重启应用。只监听 127.0.0.1，不对局域网开放。"
            ctrl={
              <input
                className="port-input"
                value={portDraft}
                inputMode="numeric"
                onChange={(e) => setPortDraft(e.target.value.replace(/[^0-9]/g, ""))}
                onBlur={() => void commitPort()}
                onKeyDown={(e) => e.key === "Enter" && void commitPort()}
              />
            }
          />
          <Row
            title="扣费备选账号"
            desc="只在这批账号里选号扣费；全选 = 全部可用（以后新增的账号也会自动参与）。选号规则固定：额度最早到期优先，同日到期则待过期积分少者优先。"
            ctrl={
              <button
                className="billing-trigger"
                disabled={busy}
                title="选择参与扣费的账号"
                onClick={openBillPick}
              >
                <IconUsers size={14} />
                {billingSet === null
                  ? `全部账号（${accounts.length}）`
                  : `已选 ${billCount} / ${accounts.length}`}
                <IconChevron size={13} className="billing-chevron" />
              </button>
            }
          />
          <Row
            title="可用模型"
            desc="上游账号池当前支持的对话模型。点开可查倍率、复制模型名；接入 Codex 时也从这份清单里勾选。"
            ctrl={
              <button
                className="billing-trigger"
                title="查看上游可用模型清单"
                onClick={openModels}
              >
                <IconList size={14} />
                {models.length > 0
                  ? `${models.length} 个模型`
                  : modelBusy
                    ? "拉取中…"
                    : "查看清单"}
                <IconChevron size={13} className="billing-chevron" />
              </button>
            }
          />
        </div>
      </div>

      <div className="card">
        <div className="card-head">
          <span>接入 Codex</span>
          <span
            className={`badge ${
              codex?.connected ? "badge-ok" : "badge-idle"
            }`}
          >
            {!codex
              ? "未知"
              : !codex.found
                ? "未装 Codex"
                : codex.connected
                  ? "已接入"
                  : "未接入"}
          </span>
        </div>
        <div className="set-row-desc">
          把本网关写进 Codex 的 <code className="mono">config.toml</code>，并在它自己的{" "}
          <code className="mono">/model</code> 选择器里列出你勾的模型 —— 之后在 Codex 里正常对话，
          扣的就是这个账号池的积分。<b>只改网关相关的那几个键</b>：它的登录、别的 provider、
          以及你原有的其它配置一字不动。
        </div>

        {codex?.connected ? (
          <>
            <div className="set-group">
              <Row
                title="服务地址"
                desc="Codex 的每个请求都打到这里（只在本机监听）。"
                ctrl={<code className="mono">{codex.baseUrl || "（未读到）"}</code>}
              />
              <Row
                title="默认模型"
                desc="Codex 启动时默认选中的那个，进去之后也能换。"
                ctrl={<code className="mono">{codex.model || "—"}</code>}
              />
              <Row
                title="已接入的模型"
                desc={codex.catalogModels.join("、") || "（模型目录是空的）"}
                ctrl={<span className="badge badge-ok">{codex.catalogModels.length} 个</span>}
              />
            </div>
            {codexRunning && (
              <div className="note warn">
                <IconAlertTriangle />
                <span>
                  Codex 正在运行，而它<b>只在启动时读一次配置</b> —— 点「重启 Codex」才会用上
                  刚写进去的配置。
                </span>
              </div>
            )}
          </>
        ) : (
          <>
            <div className="note info">
              <IconPlug />
              <span>
                输入框里那个地址和 Key 是给任意 OpenAI 兼容客户端的（Cursor、Cline、自己的脚本）；
                Codex 点下面的按钮自动写。若 Codex <b>卡在启动</b>、转圈不动，先确认这台机器能访问{" "}
                <code className="mono">chatgpt.com</code> —— 它启动时要向那里取账号设置，
                取不到就会一直等（与本网关无关）。
              </span>
            </div>
            {modelErr ? (
              <div className="note danger">
                <IconAlertTriangle />
                <span>{modelErr}</span>
              </div>
            ) : models.length === 0 ? (
              <div className="note warn">
                <IconAlertTriangle />
                <span>
                  {modelBusy
                    ? "正在拉取上游模型清单…"
                    : "还没拉到模型清单：先打开网关开关，并确保至少有一个可用账号。"}
                </span>
              </div>
            ) : null}
          </>
        )}

        {codex && !codex.found && (
          <div className="note warn">
            <IconAlertTriangle />
            <span>
              没有找到 Codex 的配置目录（
              <code className="mono">{codex.configPath || "~/.codex"}</code>
              ）。先安装并运行一次 Codex，再回来接入。
            </span>
          </div>
        )}

        <div className="card-actions">
          {codex?.connected ? (
            <>
              <button
                className="btn small primary"
                disabled={codexBusy}
                onClick={() => void doCodexRestart()}
              >
                <IconPower size={14} />
                重启 Codex
              </button>
              <button className="btn small" disabled={codexBusy} onClick={openCodexPick}>
                换模型
              </button>
              <button
                className="btn small danger"
                disabled={codexBusy}
                onClick={() => void doCodexDisconnect()}
              >
                断开
              </button>
            </>
          ) : (
            <>
              <button
                className="btn small primary"
                disabled={codexBusy || models.length === 0 || !codex?.found || !listening}
                title={
                  !codex?.found
                    ? "先安装并运行一次 Codex"
                    : !listening
                      ? "先打开上面的网关开关"
                      : "选择要写进 Codex 的模型"
                }
                onClick={openCodexPick}
              >
                <IconPlug size={14} />
                选择模型并接入
              </button>
              <button
                className="btn small"
                disabled={modelBusy}
                onClick={() => void refreshModels()}
              >
                {modelBusy ? "拉取中…" : "刷新清单"}
              </button>
            </>
          )}
        </div>
      </div>

      {/* ── 可用模型弹窗（只读清单：查倍率、复制模型名） ── */}
      {modelsOpen && (
        <Dialog
          size="md"
          icon={<IconList size={16} />}
          title="可用模型"
          subtitle="上游账号池当前支持的对话模型。复制模型名填进任意 OpenAI 兼容客户端；接入 Codex 时也从这份清单里勾选。"
          label="可用模型"
          fixedBodyHeight={420}
          onClose={() => setModelsOpen(false)}
          tools={<span className="billing-count">共 {models.length} 个</span>}
          footer={
            <>
              <button
                className="btn small"
                disabled={modelBusy}
                onClick={() => void refreshModels()}
              >
                <IconRefresh size={13} className={modelBusy ? "spin" : ""} />
                {modelBusy ? "刷新中…" : "刷新清单"}
              </button>
              <button className="btn primary" onClick={() => setModelsOpen(false)}>
                关闭
              </button>
            </>
          }
        >
          <div className="modal-fill">
            {modelErr ? (
              <div className="note danger">
                <IconAlertTriangle />
                <span>{modelErr}</span>
              </div>
            ) : models.length === 0 ? (
              <p className="empty">
                {modelBusy
                  ? "正在拉取上游模型清单…"
                  : "还没拉到模型清单：先打开网关开关，并确保至少有一个可用账号。"}
              </p>
            ) : (
              <>
                <input
                  type="text"
                  placeholder="按模型名或 slug 过滤…"
                  value={modelQuery}
                  onChange={(e) => setModelQuery(e.target.value)}
                />
                {modelFiltered.length === 0 ? (
                  <p className="empty">没有匹配「{modelQuery}」的模型。</p>
                ) : (
                  <ul className="pick-list">
                    {modelFiltered.map((m) => (
                      <li key={m.slug} className="pick-item static">
                        <span className="pick-main">
                          <span className="pick-name">
                            {m.name}
                            <span className="mono pick-slug">{m.slug}</span>
                          </span>
                        </span>
                        <span className="pick-tail">
                          <span className={"pick-state " + (m.free ? "ok" : "warn")}>
                            {m.free ? "免费" : m.credits.trim() || "倍率未知"}
                          </span>
                          <button
                            className="btn small"
                            onClick={() => void copy(m.slug, "模型名")}
                          >
                            复制
                          </button>
                        </span>
                      </li>
                    ))}
                  </ul>
                )}
              </>
            )}
          </div>
        </Dialog>
      )}

      {/* ── 接入 Codex 弹窗（草稿制：点「接入」才写文件） ── */}
      {codexOpen && (
        <Dialog
          size="md"
          icon={<IconPlug size={16} />}
          title={codex?.connected ? "更换 Codex 模型" : "接入 Codex"}
          subtitle="勾选的模型会写进 Codex 的模型目录，并出现在它的 /model 选择器里；没勾的不会出现。"
          label="选择要接入 Codex 的模型"
          fixedBodyHeight={390}
          onClose={() => setCodexOpen(false)}
          tools={
            <div className="billing-tools">
              <label className="billing-all">
                <input
                  type="checkbox"
                  checked={models.length > 0 && codexDraft.length === models.length}
                  ref={(el) => {
                    if (el)
                      el.indeterminate =
                        codexDraft.length > 0 && codexDraft.length < models.length;
                  }}
                  onChange={() =>
                    setCodexDraft(
                      codexDraft.length === models.length ? [] : models.map((m) => m.slug),
                    )
                  }
                />
                全选
              </label>
              <span className="billing-count">
                已选 {codexDraft.length} / {models.length}
              </span>
              <label className="billing-all">
                默认
                <select
                  className="codex-default"
                  value={codexPicked.some((m) => m.slug === codexDefault) ? codexDefault : ""}
                  onChange={(e) => setCodexDefault(e.target.value)}
                >
                  {codexPicked.length === 0 && <option value="">（先勾模型）</option>}
                  {codexPicked.map((m) => (
                    <option key={m.slug} value={m.slug}>
                      {m.name || m.slug}
                    </option>
                  ))}
                </select>
              </label>
            </div>
          }
          footer={
            <>
              <button className="btn" onClick={() => setCodexOpen(false)}>
                取消
              </button>
              <button
                className="btn primary"
                disabled={codexBusy || codexDraft.length === 0}
                onClick={() => void doCodexConnect()}
              >
                {codexBusy ? "写入中…" : codex?.connected ? "保存" : "接入"}
              </button>
            </>
          }
        >
          <div className="modal-fill">
            <input
              type="text"
              placeholder="按模型名或 slug 过滤…"
              value={codexQuery}
              onChange={(e) => setCodexQuery(e.target.value)}
            />
            {codexFiltered.length === 0 ? (
              <p className="empty">没有匹配「{codexQuery}」的模型。</p>
            ) : (
              <div className="billing-list">
                {codexFiltered.map((m) => {
                  const on = codexDraft.includes(m.slug);
                  return (
                    <label key={m.slug} className={"billing-row" + (on ? " picked" : "")}>
                      <input
                        type="checkbox"
                        checked={on}
                        onChange={() =>
                          setCodexDraft(
                            on
                              ? codexDraft.filter((s) => s !== m.slug)
                              : [...codexDraft, m.slug],
                          )
                        }
                      />
                      <span className="pick-main">
                        <span className="pick-name">
                          {m.name}
                          <span className="mono pick-slug">{m.slug}</span>
                        </span>
                      </span>
                      <span className="billing-meta">
                        {m.free ? "免费" : m.credits.trim() || "倍率未知"}
                      </span>
                    </label>
                  );
                })}
              </div>
            )}
          </div>
        </Dialog>
      )}

      {/* ── 扣费账号弹窗（草稿制：点「保存」才生效） ── */}
      {billOpen && (
        <Dialog
          size="md"
          icon={<IconUsers size={16} />}
          title="参与扣费"
          subtitle="未勾选的账号不参与扣费；全选时以后新增的账号也会自动参与。"
          label="选择参与扣费的账号"
          fixedBodyHeight={390}
          onClose={() => setBillOpen(false)}
          tools={
            <div className="billing-tools">
              <label className="billing-all">
                <input
                  type="checkbox"
                  checked={accounts.length > 0 && billDraft.length === accounts.length}
                  ref={(el) => {
                    if (el)
                      el.indeterminate =
                        billDraft.length > 0 && billDraft.length < accounts.length;
                  }}
                  onChange={() =>
                    setBillDraft(billDraft.length === accounts.length ? [] : [...allIds])
                  }
                />
                全选
              </label>
              <span className="billing-count">
                已选 {billDraft.length} / {accounts.length}
              </span>
            </div>
          }
          footer={
            <>
              <button className="btn" onClick={() => setBillOpen(false)}>
                取消
              </button>
              <button
                className="btn primary"
                disabled={busy}
                onClick={() => void saveBillPick()}
              >
                {busy ? "保存中…" : "保存"}
              </button>
            </>
          }
        >
          <div className="modal-fill">
            <input
              type="text"
              placeholder="按用户名或手机号过滤…"
              value={billQuery}
              onChange={(e) => setBillQuery(e.target.value)}
            />
            {accounts.length === 0 ? (
              <p className="empty">还没有账号。先到「账号签到」页登录或导入账号。</p>
            ) : billFiltered.length === 0 ? (
              <p className="empty">没有匹配「{billQuery}」的账号。</p>
            ) : (
              <div className="billing-list">
                {billFiltered.map((a) => {
                  const on = billDraft.includes(a.id);
                  const left = creditsOf(book, a.id);
                  const day = expiryDay(expiryOf(book, a.id));
                  return (
                    <label key={a.id} className={"billing-row" + (on ? " picked" : "")}>
                      <input
                        type="checkbox"
                        checked={on}
                        onChange={() =>
                          setBillDraft(
                            on ? billDraft.filter((id) => id !== a.id) : [...billDraft, a.id],
                          )
                        }
                      />
                      <span className="pick-main">
                        <span className="pick-name">{accountLabel(a.name, a.phone)}</span>
                      </span>
                      <span className="billing-meta">
                        {left == null ? "积分未知" : `剩余 ${left}`}
                        {day ? ` · ${day} 到期` : ""}
                      </span>
                    </label>
                  );
                })}
              </div>
            )}
          </div>
        </Dialog>
      )}

    </div>
  );
}
