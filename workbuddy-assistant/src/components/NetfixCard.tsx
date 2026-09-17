import { useEffect, useState } from "react";
import type { NetReport, NetStep } from "../types";
import { netDiagnose, netRestore, revealPath } from "../api";
import { baseName, type ConfirmReq, type Toast } from "../common";
import {
  IconActivity,
  IconAlertTriangle,
  IconCircleCheck,
  IconWrench,
} from "./Icons";
import { Row } from "./SettingsControls";

/**
 * 「网络急救」卡片——嵌在设置页里。
 *
 * 它排查的是 WorkBuddy 的**全局**网络残留配置（settings.json 的 env、launchd 全局环境变量），
 * 与签到设置无关，原先为了「出问题时能直接找到」独立成一页；现按「诊断类工具收进设置」的
 * 导航约定并入设置页：能力与展示全部保留，只是不再占一个侧边栏位置。
 */
export function NetfixCard({
  askConfirm,
  onReloadSettings,
  onToast,
}: {
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 恢复流程会安全关闭智能接管并落盘，外层 settings 必须跟上 */
  onReloadSettings: () => Promise<void>;
  onToast: (t: Toast) => void;
}) {
  const [netReport, setNetReport] = useState<NetReport | null>(null);
  const [netSteps, setNetSteps] = useState<NetStep[] | null>(null);
  const [netBackups, setNetBackups] = useState<string[]>([]);
  const [netBusy, setNetBusy] = useState<"diag" | "restore" | null>(null);

  const doDiagnose = async () => {
    setNetBusy("diag");
    try {
      setNetReport(await netDiagnose());
      // 上一次的恢复轨迹已经过时了，清掉免得和新的诊断结果混在一起
      setNetSteps(null);
      setNetBackups([]);
    } catch (e) {
      onToast({ kind: "err", text: "诊断失败：" + String(e) });
    } finally {
      setNetBusy(null);
    }
  };

  const doRestore = async () => {
    const ok = await askConfirm({
      title: "一键恢复网络配置",
      body:
        "会依次：安全关闭智能接管并重启 WorkBuddy/CLI host、备份并清除全局调试端点、" +
        "取消 launchd 全局环境变量，最后复检。被改动的文件都会先备份。继续？",
      okText: "恢复",
      danger: true,
    });
    if (!ok) return;
    setNetBusy("restore");
    try {
      const rep = await netRestore();
      // 接管可能是这次恢复关掉的（后端已落盘 proxy_enabled=false），外层状态要跟上
      await onReloadSettings();
      setNetReport(rep.report);
      setNetSteps(rep.steps);
      setNetBackups(rep.backups);
      const failed = rep.steps.filter((s) => !s.ok).length;
      onToast(
        failed
          ? { kind: "err", text: `恢复完成，但有 ${failed} 步失败，见下方详情` }
          : { kind: "ok", text: "已恢复：残留配置已清除" }
      );
    } catch (e) {
      onToast({ kind: "err", text: "恢复失败：" + String(e) });
    } finally {
      setNetBusy(null);
    }
  };

  useEffect(() => {
    // 卡片一出现就扫一遍（只读）：有问题不用先点「诊断」
    void doDiagnose();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="set-card card">
      <div className="set-card-head">
        <span className="set-card-icon">
          <IconActivity size={20} />
        </span>
        <div>
          <div className="set-card-title">网络急救</div>
          <div className="set-card-sub">
            排查并清除影响整个 WorkBuddy 的全局网络残留配置
          </div>
        </div>
      </div>

      <div className="set-group">
        <Row
          title="网络链路诊断"
          desc={
            "调试「自定义服务端点」时，如果把地址写进了 WorkBuddy 的全局配置" +
            "（~/.workbuddy/settings.json 的 env，或 launchd 全局环境变量），" +
            "受影响的会是整个 WorkBuddy，典型表现是「502 连接被拒绝」。"
          }
          ctrl={
            <>
              <button
                className="btn small"
                disabled={netBusy !== null}
                onClick={() => void doDiagnose()}
              >
                {netBusy === "diag" ? (
                  <>
                    <IconWrench size={15} className="spin" />
                    扫描中…
                  </>
                ) : (
                  <>
                    <IconWrench size={15} />
                    重新诊断
                  </>
                )}
              </button>
              <button
                className="btn small danger"
                disabled={netBusy !== null}
                onClick={() => void doRestore()}
              >
                <IconAlertTriangle size={15} />
                {netBusy === "restore" ? "恢复中…" : "一键恢复"}
              </button>
            </>
          }
        />

        {netReport && (
          <div className="netfix-body">
            {netReport.healthy && netReport.issues.length === 0 ? (
              <div className="status-banner ok">
                <IconCircleCheck size={18} />
                <div>
                  <strong>网络链路正常</strong>
                  <span>未发现残留端点配置，WorkBuddy 的网络链路是干净的。</span>
                </div>
              </div>
            ) : (
              <>
                <div className="status-banner warn">
                  <IconAlertTriangle size={18} />
                  <div>
                    <strong>发现 {netReport.issues.length} 项配置残留</strong>
                    <span>以下项可能影响 WorkBuddy 网络连接，建议点「一键恢复」清除。</span>
                  </div>
                </div>
                <ul className="net-list">
                  {netReport.issues.map((it) => (
                    <li key={it.id} className={`net-item ${it.level}`}>
                      <div className="net-head">
                        <span
                          className={`badge ${
                            it.level === "block"
                              ? "badge-err"
                              : it.level === "ok"
                              ? "badge-ok"
                              : "badge-already"
                          }`}
                        >
                          {it.level === "block"
                            ? "会断网"
                            : it.level === "ok"
                            ? "正常"
                            : "残留"}
                        </span>
                        <span className="net-scope">{it.scope}</span>
                        <span className="net-target">{it.target}</span>
                      </div>
                      <div className="net-value">{it.value}</div>
                      <div className="net-note">
                        {it.note}
                        {!it.fixable && "（此项只报告，需要你手动处理）"}
                      </div>
                    </li>
                  ))}
                </ul>
                {netReport.issues.some((i) => i.fixable && i.level !== "ok") && (
                  <p className="hint">
                    「一键恢复」会清除上表中标记为可自动处理的项；改动前一律先备份。
                  </p>
                )}
              </>
            )}
          </div>
        )}

        {netSteps && (
          <div className="netfix-body">
            <div className="net-steps-title">恢复详情</div>
            <ul className="net-steps">
              {netSteps.map((s, i) => (
                <li
                  key={`${i}-${s.action}`}
                  className={`net-step ${s.ok ? "ok" : "bad"}`}
                >
                  <span className="mark">{s.ok ? "✓" : "✕"}</span>
                  <span>
                    <strong>{s.action}</strong>：{s.detail}
                  </span>
                </li>
              ))}
            </ul>
            {netBackups.length > 0 && (
              <div className="net-backups">
                <span>已备份：</span>
                {netBackups.map((b) => (
                  <button
                    key={b}
                    className="link-btn"
                    title={b}
                    onClick={() =>
                      void revealPath(b).catch((e) =>
                        onToast({ kind: "err", text: String(e) })
                      )
                    }
                  >
                    {baseName(b)}
                  </button>
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
