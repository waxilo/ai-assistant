import { memo, useState } from "react";
import type { Settings } from "../types";
import { checkAndInstall, mb, percentOf, type UpdateProgress } from "../updater";
import type { Toast } from "../common";
import Switch from "../components/Switch";
import { Row } from "../components/SettingsControls";
import { IconCalendar, IconInfo, IconRefresh } from "../components/Icons";

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
  onToast: (t: Toast) => void;
}

function SettingsPage({ settings, update, version, onToast }: Props) {
  const [progress, setProgress] = useState<UpdateProgress | null>(null);
  const [busy, setBusy] = useState(false);

  const onUpdate = async () => {
    if (busy) return;
    setBusy(true);
    setProgress({ status: "checking", message: "正在检查更新…" });
    await checkAndInstall((p) => {
      setProgress(p);
      if (p.status === "error") onToast({ kind: "err", text: p.message });
      if (p.status === "no-update") onToast({ kind: "info", text: p.message });
    });
    setBusy(false);
  };

  /** 下载百分比；总量未知（服务端没给 Content-Length）时为 null → 走不确定态动画 */
  const pct = percentOf(progress);
  const downloading =
    progress?.status === "downloading" || progress?.status === "installing";
  const checkinOn = !!settings?.checkin_enabled;

  return (
    <>
      <section className="set-card card">
        <div className="set-card-head">
          <span className="set-card-icon">
            <IconCalendar size={19} />
          </span>
          <div>
            <div className="set-card-title">签到与通知</div>
            <div className="set-card-sub">定时签到的开关与失败提醒</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="启用定时签到"
            desc="应用保持运行时才会触发；每次签到的结果都会写进「签到日志」。"
            ctrl={
              <Switch
                checked={checkinOn}
                onChange={(v) => update({ checkin_enabled: v })}
                title="每天按设定时刻对全部账号签到一次"
              />
            }
          />
          {checkinOn && (
            <Row
              sub
              title="定时签到时刻"
              desc="到达这个时间点后由后台线程执行，不要求界面停在某一页。"
              ctrl={
                <input
                  type="time"
                  value={settings?.checkin_time || "10:00"}
                  onChange={(e) => update({ checkin_time: e.target.value })}
                />
              }
            />
          )}
          <Row
            title="失败通知地址"
            desc="Webhook 地址（GET 即可）。签到失败时会汇总成一条消息推过去；留空则不通知。"
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
            <IconInfo size={19} />
          </span>
          <div>
            <div className="set-card-title">关于与更新</div>
            <div className="set-card-sub">版本信息与自动更新</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="当前版本"
            desc="更新从 GitHub Release 拉取已签名的新版本并自动安装、重启。"
            ctrl={<span className="set-readonly">v{version || "…"}</span>}
          />
          <Row
            title="检查更新"
            desc="若提示「已经是最新版本」，说明当前已是最新。"
            ctrl={
              <>
                <button className="btn small" disabled={busy} onClick={() => void onUpdate()}>
                  <IconRefresh size={14} className={busy ? "spin" : undefined} />
                  {busy ? "处理中…" : "检查更新"}
                </button>
              </>
            }
          />
          {progress && (
            <div className="set-row in-expand">
              <div className="set-row-main">
                <div
                  className={`upd-status${
                    progress.status === "error" ? " err" : progress.status === "updated" ? " ok" : ""
                  }`}
                >
                  {progress.message}
                </div>
                {/* 下载/安装进度条：总量已知给百分比，未知则走不确定态动画 */}
                {downloading && (
                  <div className="upd-row">
                    <div className="upd-progress-wrap">
                      <div
                        className={`upd-progress-bar${pct === null ? " indet" : ""}`}
                        style={pct === null ? undefined : { width: `${pct}%` }}
                      />
                    </div>
                    <div className="upd-progress-text">
                      {pct === null
                        ? `已下载 ${mb(progress.downloaded ?? 0)} MB`
                        : `${pct}% · ${mb(progress.downloaded ?? 0)} / ${mb(progress.total ?? 0)} MB`}
                    </div>
                  </div>
                )}
              </div>
            </div>
          )}
        </div>
      </section>
    </>
  );
}

export default memo(SettingsPage);
