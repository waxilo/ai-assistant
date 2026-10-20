import { memo, useState } from "react";
import type { Settings } from "../types";
import { downloadProgress, formatBytes, type UpdateProgress } from "../updater";
import Switch from "../components/Switch";
import { Row } from "../components/SettingsControls";
import { FONT_OPTIONS, getFontKey, setFontKey } from "../font";
import { IconCalendar, IconDownload, IconType } from "../components/Icons";

/**
 * 「设置」页：定时签到与失败通知。
 *
 * 取向上有两处刻意的选择：
 *
 * 1. **改动即时落盘，没有「保存」按钮**（`update` 直接写内存 + 落盘）。多一个保存按钮就多
 *    一个「改完忘了点」的状态，而这一页全是幂等的开关与短文本，没有需要成组提交的理由。
 *    因此页面上也不弹「保存成功」——每次改动都弹会刷屏，只有失败才出声。
 * 2. 卡片用**左右分栏**：左栏说明「这一组是干什么的」，右栏每条设置左右对齐。
 *    旧版是在一个 `<div className="form">` 里裸排 label，连「一行」这个概念都没有，
 *    于是「启用定时签到」和「定时签到时刻」在视觉上是两个同级东西 ——
 *    而现在时刻是 **sub 行**（浅底、紧跟开关），从属关系一眼可见。
 *
 * ⚠️ 接管相关字段（`takeover_*` / `billing_account_ids`）本页没有编辑权：
 * `update` 是基于最新 settings 的浅合并，所以本页的改动不会把接管页的选择覆盖掉。
 */
interface Props {
  settings: Settings | null;
  update: (patch: Partial<Settings>) => void;
  version: string;
  /** 全局更新状态（App 持有，下载是整机动作，切页进度不丢） */
  updateStatus: UpdateProgress | null;
  updateBusy: boolean;
  /** 触发检查并安装（实现与状态都在 App 层） */
  onRunUpdate: () => void;
  /** 后台轮询查到的版本号（就是点亮侧边栏黄点的那条），这里用于打开页面就有提示 */
  updateVersion: string | null;
}

function SettingsPage({
  settings,
  update,
  version,
  updateStatus,
  updateBusy,
  onRunUpdate,
  updateVersion,
}: Props) {
  // 界面字体：纯前端偏好，即时生效并持久化（见 src/font.ts），不经过后端 settings
  const [fontKey, setFontKeyState] = useState(() => getFontKey());

  const onUpdate = () => onRunUpdate();

  /** 下载进度：percent 为 null = 总量未知 / 不在下载阶段（两者都不画条） */
  const dl = downloadProgress(updateStatus);
  const checkinOn = !!settings?.checkin_enabled;
  /**
   * 状态行文案：后台轮询到的版本也放进这一行。
   * 卡片高度必须与「有没有点过检查」无关，所以版本信息不能另起一行。
   */
  const updMsg =
    updateStatus?.message ??
    (updateVersion ? `发现新版本 v${updateVersion}，建议立即更新。` : "");
  const updTone =
    updateStatus?.status === "error"
      ? " err"
      : updateStatus?.status === "updated" || updateStatus?.status === "no-update"
      ? " ok"
      : "";

  return (
    <>
      <section className={`set-card card${updateVersion ? " highlight" : ""}`}>
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconDownload size={19} />
          </span>
          <div>
            <div className="set-card-title">应用更新</div>
            <div className="set-card-sub">自动检查并安装新版本</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title={version ? `当前版本 v${version}` : "当前版本"}
            desc="有新版本时会在这里提醒。"
            ctrl={
              <button
                className={`btn small${updateVersion ? " primary" : ""}`}
                disabled={updateBusy}
                onClick={() => void onUpdate()}
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
                    <div className="upd-progress-bar" style={{ width: `${dl.percent}%` }} />
                  </div>
                )}
                <span className="upd-progress-text">
                  {dl.percent === null
                    ? `已下载 ${formatBytes(dl.downloaded)}`
                    : `${formatBytes(dl.downloaded)} / ${formatBytes(dl.total)} · ${dl.percent}%`}
                </span>
              </>
            )}
          </div>
        </div>
      </section>

      <section className="set-card card">
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconCalendar size={19} />
          </span>
          <div>
            <div className="set-card-title">签到与通知</div>
            <div className="set-card-sub">定时签到与失败提醒</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="启用定时签到"
            desc="应用运行时才会执行。"
            ctrl={
              <Switch
                checked={checkinOn}
                onChange={(v) => update({ checkin_enabled: v })}
                title="每天按设定时刻对全部账号签到一次"
              />
            }
          />
          {/* 这一行常驻、关掉开关时只禁用：条件挂载会让整张卡片忽高忽低 */}
          <Row
            sub
            title="定时签到时刻"
            desc="到点自动执行，不需要停留在本页。"
            ctrl={
              <input
                type="time"
                value={settings?.checkin_time || "10:00"}
                disabled={!checkinOn}
                onChange={(e) => update({ checkin_time: e.target.value })}
              />
            }
          />
          <Row
            title="失败通知地址"
            desc="签到结果会推送一条；需要处理的失败会单独提醒。留空则不通知。"
            ctrl={
              <input
                className="set-input"
                type="text"
                placeholder="留空则不通知"
                value={settings?.webhook_url || ""}
                onChange={(e) => update({ webhook_url: e.target.value })}
              />
            }
          />
        </div>
      </section>

      <section className="set-card card">
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconType size={19} />
          </span>
          <div>
            <div className="set-card-title">界面外观</div>
            <div className="set-card-sub">界面字体</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="界面字体"
            desc="只影响本机界面显示。"
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
      </section>
    </>
  );
}

export default memo(SettingsPage);
