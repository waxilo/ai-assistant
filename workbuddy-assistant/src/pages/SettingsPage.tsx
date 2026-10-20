import { useEffect, useRef, useState } from "react";
import type { Settings } from "../types";
import { setAutostart, getAutostart, testNotify } from "../api";
import { downloadProgress, type UpdateProgress } from "../updater";
import { formatBytes, type ConfirmReq, type Toast } from "../common";
import { FONT_OPTIONS, getFontKey, setFontKey } from "../font";
import {
  IconCalendar,
  IconShield,
  IconBell,
  IconDownload,
  IconType,
} from "../components/Icons";
import { Row } from "../components/SettingsControls";
import Switch from "../components/Switch";
import { NetfixCard } from "../components/NetfixCard";

/**
 * 「设置」页：定时签到 / 通知 / 风控 / 自启动等常规配置。
 *
 * 卡片式分组布局：每个主题一张卡，每条设置左右分栏（左侧标题 + 说明，右侧控件），
 * 开关统一用 .switch 切换。网络急救作为一张卡收敛在本页底部（见 NetfixCard），
 * 智能接管仍独立成页；接管相关字段（proxy_* / billing_account_ids）本页没有编辑权，保存时原样透传。
 *
 * 简报的**开关**在「积分简报」页（那一页才是它的主场），本页只留它的
 * **推送开关**并与两条签到通知并排 —— 「哪些东西会推送」集中在一处才看得清。
 */
export function SettingsPage({
  version,
  settings,
  onSave,
  onToast,
  askConfirm,
  onReloadSettings,
  updateStatus,
  updateBusy,
  onRunUpdate,
  updateVersion,
}: {
  version: string;
  settings: Settings;
  onSave: (s: Settings) => Promise<void>;
  onToast: (t: Toast) => void;
  /** 网络急救的「一键恢复」是危险操作，复用全局自研确认框 */
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 恢复流程会安全关闭智能接管并落盘，需要重拉一份 settings 覆盖本地草稿 */
  onReloadSettings: () => Promise<void>;
  /** 全局更新状态（App 持有，下载是整机动作，切页进度不丢） */
  updateStatus: UpdateProgress | null;
  updateBusy: boolean;
  /** 触发检查并安装（实现与状态都在 App 层） */
  onRunUpdate: () => void;
  /** 后台轮询查到的版本号（就是点亮侧边栏红点的那条），这里用于打开页面就有提示 */
  updateVersion: string | null;
}) {
  const [auto, setAuto] = useState(settings.auto_checkin_on_start);
  // 界面字体：纯前端偏好，即时生效并持久化（见 src/font.ts），不经过后端 settings
  const [fontKey, setFontKeyState] = useState(() => getFontKey());
  const [schedOn, setSchedOn] = useState(settings.schedule_enabled);
  const [schedTime, setSchedTime] = useState(settings.schedule_time);
  // 定时签到的随机时间窗（分钟）：0 = 关闭随机，精确到设定时刻
  const [schedWindow, setSchedWindow] = useState(
    String(settings.schedule_window_minutes)
  );
  const [notifyOn, setNotifyOn] = useState(settings.notify_enabled);
  const [webhook, setWebhook] = useState(settings.notify_webhook);
  const [notifySched, setNotifySched] = useState(settings.notify_on_schedule);
  const [notifyManual, setNotifyManual] = useState(settings.notify_on_manual);
  // 积分简报：后台每小时结算时条目，推送按**天**走。**简报本身的开关在「积分简报」页**，
  // 这里只留「生成后推送」——它与两条签到通知开关并排，一眼看清「哪些会推送」。
  const [notifyReport, setNotifyReport] = useState(settings.notify_on_briefing);
  // 多账号风控：批量签到时账号之间随机歇几秒，避免同 IP 瞬时连发
  const [staggerOn, setStaggerOn] = useState(settings.stagger_checkin);
  const [staggerMax, setStaggerMax] = useState(String(settings.stagger_max_seconds));
  // 顺序打散 + 手动路径节流：前者抹掉「固定先后」，后者堵住「点一下就连发」这个缺口
  const [shuffle, setShuffle] = useState(settings.shuffle_checkin_order);
  const [manualStagger, setManualStagger] = useState(settings.manual_stagger);
  const [manualMax, setManualMax] = useState(
    String(settings.manual_stagger_max_seconds)
  );
  const [testing, setTesting] = useState(false);
  const [err, setErr] = useState("");
  // 开机自启动是「操作系统状态」，不属于 settings.json，改一次立即生效
  const [autostart, setAutostartOn] = useState<boolean | null>(null);
  const [autoBusy, setAutoBusy] = useState(false);

  // 当前草稿：含其它页托管的字段（proxy_* / billing_account_ids 等），原样透传，
  // 每次改动都基于它合并后自动保存，避免覆盖「智能接管」页改过的值。
  const draftRef = useRef<Settings>({ ...settings });
  const saveTimer = useRef<number | null>(null);

  // 外部来源改了 settings 时要同步草稿：网络急救的「一键恢复」会安全关闭智能接管并落盘，
  // 若草稿仍攥着旧的 proxy_enabled=true，用户随后改任意一项都会把它写回去——等于把反代「复活」。
  // 保存走的是 setSettings(服务端返回值)，这里同步到的始终是权威值，不会覆盖正在编辑的内容
  //（patch 用闭包里的 next 落盘，不依赖同步时机）。
  useEffect(() => {
    draftRef.current = { ...settings };
  }, [settings]);

  useEffect(() => {
    getAutostart()
      .then(setAutostartOn)
      .catch(() => setAutostartOn(null));
  }, []);

  const toggleAutostart = async (next: boolean) => {
    setAutoBusy(true);
    setAutostartOn(next); // 乐观更新，失败再回滚
    try {
      setAutostartOn(await setAutostart(next));
    } catch (e) {
      setAutostartOn(!next);
      onToast({ kind: "err", text: String(e) });
    } finally {
      setAutoBusy(false);
    }
  };

  // 合并改动并自动保存：防抖 400ms，避免连打字符反复写盘；
  // 任何一项改动都即时落盘，无需「保存」按钮。
  const patch = (updates: Partial<Settings>) => {
    const next = { ...draftRef.current, ...updates };
    draftRef.current = next;
    if (saveTimer.current) window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(async () => {
      try {
        await onSave(next);
        setErr("");
      } catch (e) {
        setErr(String(e));
      } finally {
        saveTimer.current = null;
      }
    }, 400);
  };

  const doTest = async () => {
    setTesting(true);
    try {
      const res = await testNotify(webhook.trim());
      onToast({ kind: "ok", text: "推送成功：" + res });
    } catch (e) {
      onToast({ kind: "err", text: "推送失败：" + String(e) });
    } finally {
      setTesting(false);
    }
  };

  const doUpdate = () => onRunUpdate();

  // 下载进度：percent 为 null = 总量未知 / 不在下载阶段（两者都不画条）
  const dl = downloadProgress(updateStatus);
  /**
   * 更新状态行：后台轮询到的版本也放进这一行。
   * 卡片高度必须与「有没有点过检查」无关，所以版本信息不另起一行。
   */
  const updMsg = updateStatus?.message ?? "";
  const updTone =
    updateStatus?.status === "error"
      ? " err"
      : updateStatus?.status === "updated" ||
        updateStatus?.status === "no-update"
      ? " ok"
      : "";

  return (
    <section className="panel-page">
      {/* 应用更新 */}
      <div className="set-card card">
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconDownload size={19} />
          </span>
          <div>
            <div className="set-card-title">应用更新</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title={
              updateVersion
                ? `当前版本 v${version} → v${updateVersion}`
                : version
                ? `当前版本 v${version}`
                : "当前版本"
            }
            ctrl={
              <button
                className={`btn small${updateVersion ? " primary" : ""}`}
                disabled={updateBusy}
                onClick={() => void doUpdate()}
              >
                {updateBusy ? "检查中…" : updateVersion ? "立即更新" : "检查更新"}
              </button>
            }
          />
          {/* 固定高度的状态槽：没有状态时也占着这一行，
              检查中 / 已是最新 / 下载中 / 失败都只在原地换字，不把卡片顶高。 */}
          <div className="upd-line" aria-live="polite">
            <span
              className={`upd-status${updTone}`}
              title={updMsg || undefined}
            >
              {updMsg}
            </span>
            {dl && (
              <>
                {/* 只有拿得到百分比才画条：总量未知时进度条没有可信的长度，
                    与其摆一条会自己动的不确定态，不如干脆不画（下面照实报字节数）。 */}
                {dl.percent !== null && (
                  <div className="upd-progress-wrap">
                    <div
                      className="upd-progress-bar"
                      style={{ width: `${dl.percent}%` }}
                    />
                  </div>
                )}
                {(dl.percent !== null || dl.downloaded > 0) && (
                  <span className="upd-progress-text">
                    {dl.percent === null
                      ? `已下载 ${formatBytes(dl.downloaded)}`
                      : `${formatBytes(dl.downloaded)} / ${formatBytes(dl.total)} · ${dl.percent}%`}
                  </span>
                )}
              </>
            )}
          </div>
        </div>
      </div>

      {/* 界面外观 */}
      <div className="set-card card">
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconType size={20} />
          </span>
          <div>
            <div className="set-card-title">界面外观</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="界面字体"
            desc="本机偏好，重新安装不受影响。"
            ctrl={
              <select
                value={fontKey}
                onChange={(e) => {
                  const k = e.target.value;
                  setFontKeyState(k);
                  setFontKey(k); // 即时生效并落盘 localStorage
                }}
              >
                {FONT_OPTIONS.map((o) => (
                  <option key={o.key} value={o.key}>
                    {o.label}
                  </option>
                ))}
              </select>
            }
          />
        </div>
      </div>

      {/* 签到自动化 */}
      <div className="set-card card">
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconCalendar size={20} />
          </span>
          <div>
            <div className="set-card-title">签到自动化</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="启动应用时自动签到"
            desc="打开应用时自动为全部账号签到一次"
            ctrl={
              <Switch
                checked={auto}
                onChange={(v) => {
                  setAuto(v);
                  patch({ auto_checkin_on_start: v });
                }}
                title="启动即签到"
              />
            }
          />
          <Row
            title="每天定时签到"
            desc="在指定时刻为全部账号自动签到"
            ctrl={
              <Switch
                checked={schedOn}
                onChange={(v) => {
                  setSchedOn(v);
                  patch({ schedule_enabled: v });
                }}
              />
            }
          />
          {/* 这一行常驻、关掉开关时只禁用：条件挂载会让整张卡片忽高忽低 */}
          <Row
            sub
            title="签到时刻"
            desc="应用运行期间触发；错过后 30 分钟内打开会自动补签。"
            ctrl={
              <input
                type="time"
                value={schedTime}
                disabled={!schedOn}
                onChange={(e) => {
                  const v = e.target.value;
                  setSchedTime(v);
                  patch({ schedule_time: v.trim() });
                }}
              />
            }
          />
          <Row
            sub
            title="随机时间窗"
            desc="在该时刻后随机一分钟触发，当天固定不变；填 0 则精确到设定时刻。"
            ctrl={
              <>
                <input
                  type="number"
                  value={schedWindow}
                  min={0}
                  max={720}
                  disabled={!schedOn}
                  onChange={(e) => {
                    const v = e.target.value;
                    setSchedWindow(v);
                    // 空串按 0（=关闭随机）处理，别用 || 落到默认值上把随机又打开
                    const n = Math.min(720, Math.max(0, Number(v) || 0));
                    patch({ schedule_window_minutes: n });
                  }}
                />
                <span className="set-suffix">分钟</span>
              </>
            }
          />
          <Row
            title="开机自启动"
            desc={
              autostart === null
                ? "当前平台不支持此设置。"
                : schedOn && !autostart
                ? "定时签到需要应用保持运行。"
                : "登录系统时自动运行本应用。"
            }
            ctrl={
              <Switch
                checked={autostart === true}
                disabled={autoBusy || autostart === null}
                onChange={(v) => void toggleAutostart(v)}
              />
            }
          />
        </div>
      </div>

      {/* 多账号风控 */}
      <div className="set-card card">
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconShield size={20} />
          </span>
          <div>
            <div className="set-card-title">多账号风控</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="账号间随机间隔"
            desc="批量签到时，账号之间随机等待，避免瞬时连发。"
            ctrl={
              <Switch
                checked={staggerOn}
                onChange={(v) => {
                  setStaggerOn(v);
                  patch({ stagger_checkin: v });
                }}
              />
            }
          />
          <Row
            sub
            title="间隔上限"
            desc="单次随机等待的上限（2～600 秒）。"
            ctrl={
              <>
                <input
                  type="number"
                  value={staggerMax}
                  min={2}
                  max={600}
                  disabled={!staggerOn}
                  onChange={(e) => {
                    const v = e.target.value;
                    setStaggerMax(v);
                    patch({ stagger_max_seconds: Number(v) || 45 });
                  }}
                />
                <span className="set-suffix">秒</span>
              </>
            }
          />
          <Row
            title="随机打乱签到顺序"
            desc="每次批量签到随机决定先后，避免固定请求序列。"
            ctrl={
              <Switch
                checked={shuffle}
                onChange={(v) => {
                  setShuffle(v);
                  patch({ shuffle_checkin_order: v });
                }}
              />
            }
          />
          <Row
            title="手动「全部签到」也加间隔"
            desc="手动批量签到时同样随机等待。"
            ctrl={
              <Switch
                checked={manualStagger}
                onChange={(v) => {
                  setManualStagger(v);
                  patch({ manual_stagger: v });
                }}
              />
            }
          />
          <Row
            sub
            title="手动间隔上限"
            desc="手动签到的随机等待上限（2～600 秒）。"
            ctrl={
              <>
                <input
                  type="number"
                  value={manualMax}
                  min={2}
                  max={600}
                  disabled={!manualStagger}
                  onChange={(e) => {
                    const v = e.target.value;
                    setManualMax(v);
                    patch({ manual_stagger_max_seconds: Number(v) || 8 });
                  }}
                />
                <span className="set-suffix">秒</span>
              </>
            }
          />
        </div>
      </div>

      {/* 签到与简报通知 */}
      <div className="set-card card">
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconBell size={20} />
          </span>
          <div>
            <div className="set-card-title">签到与简报通知</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="开启 webhook 通知"
            desc="关闭后不再推送任何通知。"
            ctrl={
              <Switch
                checked={notifyOn}
                onChange={(v) => {
                  setNotifyOn(v);
                  patch({ notify_enabled: v });
                }}
              />
            }
          />
          <Row
            sub
            title="推送地址（Webhook）"
            desc="支持任何接受 POST 的地址。"
            ctrl={
              <>
                <input
                  className="set-input"
                  value={webhook}
                  placeholder="https://…/hook/<key>"
                  disabled={!notifyOn}
                  onChange={(e) => {
                    const v = e.target.value;
                    setWebhook(v);
                    patch({ notify_webhook: v.trim() });
                  }}
                />
                <button
                  className="btn small"
                  disabled={!notifyOn || testing || !webhook.trim()}
                  onClick={() => void doTest()}
                >
                  {testing ? "发送中…" : "测试推送"}
                </button>
              </>
            }
          />
          <Row
            title="定时签到后推送"
            ctrl={
              <Switch
                checked={notifySched}
                disabled={!notifyOn}
                onChange={(v) => {
                  setNotifySched(v);
                  patch({ notify_on_schedule: v });
                }}
              />
            }
          />
          <Row
            title="手动「全部签到」后推送"
            ctrl={
              <Switch
                checked={notifyManual}
                disabled={!notifyOn}
                onChange={(v) => {
                  setNotifyManual(v);
                  patch({ notify_on_manual: v });
                }}
              />
            }
          />
          <Row
            title="积分简报生成后推送"
            desc="每天零点推一条当天汇总；应用未运行则下次启动补推。"
            ctrl={
              <Switch
                checked={notifyReport}
                disabled={!notifyOn}
                onChange={(v) => {
                  setNotifyReport(v);
                  patch({ notify_on_briefing: v });
                }}
              />
            }
          />
        </div>
      </div>

      {/* 网络急救：诊断类工具，收在设置页底部 */}
      <NetfixCard
        askConfirm={askConfirm}
        onReloadSettings={onReloadSettings}
        onToast={onToast}
      />

      {err && <p className="form-err">{err}</p>}
    </section>
  );
}
