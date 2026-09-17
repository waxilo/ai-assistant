import { useEffect } from "react";
import type { ConfirmReq } from "../common";
import { Dialog } from "./Dialog";
import { IconAlertTriangle, IconInfo } from "./Icons";

/**
 * 自研确认框：替代 window.confirm。
 *
 * Tauri 的 WebView 未实现原生 confirm 面板，调用会静默返回 false ——
 * 「删除账号」「清空日志」这类操作会永远不执行，而且界面上看不出任何原因。
 *
 * 遮罩、Esc 关闭、dialog 语义与滚动锁都交给 Dialog —— 这里只额外负责
 * 「Enter 确认」（普通弹窗按 Enter 不该提交）与 danger 语义（危险操作给
 * 头部图标座换成红底警告三角，并让确定键变红）。
 */
export function ConfirmDialog({
  req,
  onDone,
}: {
  req: ConfirmReq;
  onDone: (ok: boolean) => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Enter") onDone(true);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onDone]);

  return (
    <Dialog
      size="sm"
      tone={req.danger ? "danger" : "plain"}
      icon={req.danger ? <IconAlertTriangle size={16} /> : <IconInfo size={16} />}
      title={req.title}
      label={req.title}
      maskClassName="confirm-mask"
      onClose={() => onDone(false)}
      footer={
        <>
          <button className="btn ghost" autoFocus onClick={() => onDone(false)}>
            取消
          </button>
          <button
            className={req.danger ? "btn danger" : "btn primary"}
            onClick={() => onDone(true)}
          >
            {req.okText ?? "确定"}
          </button>
        </>
      }
    >
      {req.body && <p className="confirm-body">{req.body}</p>}
    </Dialog>
  );
}
