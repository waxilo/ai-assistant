import { useEffect } from "react";
import type { ConfirmReq } from "../common";
import { Dialog } from "./Dialog";
import { IconAlertTriangle, IconInfo } from "./Icons";

/**
 * 自研确认框：替代 window.confirm（Tauri 的 WKWebView 不支持原生 confirm 面板）。
 *
 * 外形与行为都交给 Dialog（三段式外壳、遮罩、Esc、语义、滚动锁）—— 这里只负责
 * 确认框独有的三件事：
 *
 * 1. **Enter 确认**（普通弹窗按 Enter 不该提交）；
 * 2. **danger 语义**：危险操作给头部图标座换成红底警告三角，并让确定键变红 ——
 *    以前危险与否只体现在按钮颜色上，标题旁边没有任何提示；
 * 3. 取消键 `autoFocus`：默认焦点落在「取消」而不是危险动作上。
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
      icon={
        req.danger ? <IconAlertTriangle size={16} /> : <IconInfo size={16} />
      }
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
      {req.body && <p className="modal-text">{req.body}</p>}
    </Dialog>
  );
}
