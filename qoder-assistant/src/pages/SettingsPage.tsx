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
  IconCheck,
  IconAlertTriangle,
  IconInfo,
  IconDownload,
  IconType,
} from "../components/Icons";
import { Row, Toggle } from "../components/SettingsControls";
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
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
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
      onToast({ kind: "ok", text: next ? "已设置开机自启动" : "已关闭开机自启动" });
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
    setSaved(false);
    if (saveTimer.current) window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(async () => {
      setSaving(true);
      try {
        await onSave(next);
        setErr("");
        setSaved(true);
      } catch (e) {
        setErr(String(e));
      } finally {
        setSaving(false);
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

  return (
    <section className="panel-page">
      <p className="set-intro">
        <IconInfo size={14} />
        <span>
          定时签到、多账号风控、通知、应用更新与网络急救。智能接管相关配置请在「智能接管」页调整。
        </span>
      </p>

      {/* 界面外观 */}
      <div className="set-card card">
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconType size={20} />
          </span>
          <div>
            <div className="set-card-title">界面外观</div>
            <div className="set-card-sub">界面使用的字体，改动即时生效</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="界面字体"
            desc="选择常用字体之一；此项为本机偏好，重新安装应用不受影响。"
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
            <div className="set-card-sub">控制账号在何时自动完成签到</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="启动应用时自动签到"
            desc="打开应用时自动为全部账号签到一次"
            ctrl={
              <Toggle
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
              <Toggle
                checked={schedOn}
                onChange={(v) => {
                  setSchedOn(v);
                  patch({ schedule_enabled: v });
                }}
              />
            }
          />
          {schedOn && (
            <Row
              sub
              title="签到时刻"
              desc="24 小时制，作为时间窗的起点。应用运行期间触发；错过时刻后 30 分钟内打开会自动补签。"
              ctrl={
                <input
                  type="time"
                  value={schedTime}
                  onChange={(e) => {
                    const v = e.target.value;
                    setSchedTime(v);
                    patch({ schedule_time: v.trim() });
                  }}
                />
              }
            />
          )}
          {schedOn && (
            <Row
              sub
              title="随机时间窗"
              desc="在该时刻之后的这段时间内随机挑一分钟触发，当天挑定后不再变，并推送一条通知告知今天的实际时刻。固定在同一分钟触发是脚本最好认的特征；填 0 则精确到设定时刻。"
              ctrl={
                <>
                  <input
                    type="number"
                    value={schedWindow}
                    min={0}
                    max={720}
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
          )}
          <Row
            title="开机自启动"
            desc="登录系统时自动运行本应用（操作系统级设置）"
            ctrl={
              <Toggle
                checked={autostart === true}
                disabled={autoBusy || autostart === null}
                onChange={(v) => void toggleAutostart(v)}
              />
            }
          />
          {autostart === null ? (
            <p className="hint set-foot">未能读取开机自启动状态（该平台可能不支持）。</p>
          ) : (
            schedOn &&
            !autostart && (
              <p className="hint set-foot warn">
                <IconAlertTriangle size={13} />
                建议同时开启「开机自启动」，否则应用不运行时定时签到不会发生。
              </p>
            )
          )}
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
            <div className="set-card-sub">批量签到时打散请求节奏，降低触发风控的概率</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="账号间随机间隔"
            desc="批量签到时，每个账号之间随机等待一段时间再发下一个，避免同 IP 瞬时连发。"
            ctrl={
              <Toggle
                checked={staggerOn}
                onChange={(v) => {
                  setStaggerOn(v);
                  patch({ stagger_checkin: v });
                }}
              />
            }
          />
          {staggerOn && (
            <Row
              sub
              title="间隔上限"
              desc="每个账号之间的随机等待不超过该秒数（2～600）。单账号签到不受影响。"
              ctrl={
                <>
                  <input
                    type="number"
                    value={staggerMax}
                    min={2}
                    max={600}
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
          )}
          <Row
            title="随机打乱签到顺序"
            desc="每次批量签到都随机决定先后。固定按列表顺序连发，等于把「同一批账号」直接写进请求序列；打乱只影响发请求的次序，列表顺序不变。"
            ctrl={
              <Toggle
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
            desc="手动点击同样不该瞬时连发——过去手动路径是全程唯一没有节流的入口，风控看到的恰好就是那几秒内完成的一串签到。"
            ctrl={
              <Toggle
                checked={manualStagger}
                onChange={(v) => {
                  setManualStagger(v);
                  patch({ manual_stagger: v });
                }}
              />
            }
          />
          {manualStagger && (
            <Row
              sub
              title="手动间隔上限"
              desc="手动签到时账号之间的随机等待不超过该秒数（2～600）。比自动签到短得多，避免点一次要等太久。"
              ctrl={
                <>
                  <input
                    type="number"
                    value={manualMax}
                    min={2}
                    max={600}
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
          )}
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
            <div className="set-card-sub">签到结果与积分简报都通过同一个 webhook 推送</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="开启 webhook 通知"
            desc="关闭后不再推送任何签到通知"
            ctrl={
              <Toggle
                checked={notifyOn}
                onChange={(v) => {
                  setNotifyOn(v);
                  patch({ notify_enabled: v });
                }}
              />
            }
          />
          {notifyOn && (
            <div className="set-expand">
              <label className="set-field">
                推送地址（Webhook）
                <input
                  value={webhook}
                  placeholder="https://…/hook/<key>"
                  onChange={(e) => {
                    const v = e.target.value;
                    setWebhook(v);
                    patch({ notify_webhook: v.trim() });
                  }}
                />
              </label>
              <div className="opt-row">
                <button
                  className="btn small"
                  disabled={testing || !webhook.trim()}
                  onClick={() => void doTest()}
                >
                  {testing ? "发送中…" : "测试推送"}
                </button>
                <span className="hint inline">点一下会给这个地址发一条测试消息</span>
              </div>
              <Row
                bare
                title="定时签到后推送"
                ctrl={
                  <Toggle
                    checked={notifySched}
                    onChange={(v) => {
                      setNotifySched(v);
                      patch({ notify_on_schedule: v });
                    }}
                  />
                }
              />
              <Row
                bare
                title="手动「全部签到」后推送"
                ctrl={
                  <Toggle
                    checked={notifyManual}
                    onChange={(v) => {
                      setNotifyManual(v);
                      patch({ notify_on_manual: v });
                    }}
                  />
                }
              />
              <Row
                bare
                title="积分简报生成后推送"
                desc="每天 24:00（跨零点那一刻）推一条当天汇总；应用当时没在运行，会在下次启动时补推。简报本身的开关在「积分简报」页。"
                ctrl={
                  <Toggle
                    checked={notifyReport}
                    onChange={(v) => {
                      setNotifyReport(v);
                      patch({ notify_on_briefing: v });
                    }}
                  />
                }
              />
            </div>
          )}
        </div>
      </div>

      {/* 应用更新 */}
      <div className="set-card card">
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconDownload size={20} />
          </span>
          <div>
            <div className="set-card-title">应用更新</div>
            <div className="set-card-sub">检查并安装来自 GitHub Release 的新版本</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title={version ? `当前版本 v${version}` : "当前版本"}
            desc={
              updateVersion
                ? `后台已发现新版本 v${updateVersion}，点右侧按钮立即更新。`
                : "应用会自动检查更新，发现新版本会在侧边栏「设置」上点亮一颗小红点。"
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
          {updateStatus && (
            <div className="upd-row">
              <div
                className={`upd-status ${
                  updateStatus.status === "error"
                    ? "err"
                    : updateStatus.status === "updated"
                    ? "ok"
                    : ""
                }`}
              >
                {updateStatus.message}
              </div>
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
                  <div className="upd-progress-text">
                    {dl.percent === null
                      ? `已下载 ${formatBytes(dl.downloaded)}`
                      : `${formatBytes(dl.downloaded)} / ${formatBytes(dl.total)} · ${dl.percent}%`}
                  </div>
                </>
              )}
            </div>
          )}
        </div>
      </div>

      {/* 网络急救：诊断类工具，收在设置页底部 */}
      <NetfixCard
        askConfirm={askConfirm}
        onReloadSettings={onReloadSettings}
        onToast={onToast}
      />

      {err && <p className="form-err">{err}</p>}
      {(saving || saved) && (
        <div className="set-autosave">
          {saving ? (
            <>
              <span className="spin-dot" />
              保存中…
            </>
          ) : (
            <>
              <IconCheck size={14} />
              修改已自动保存
            </>
          )}
        </div>
      )}
    </section>
  );
}
