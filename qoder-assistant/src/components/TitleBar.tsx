import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * 自绘标题栏：空白区是窗口拖拽区，右侧是最小化 / 最大化（还原）/ 关闭。
 *
 * 拖动与双击放大/还原**不要自己挂事件**，交给 Tauri 注入的原生处理 ——
 * 空白区挂 `data-tauri-drag-region`，core 脚本在 mousedown 上按 `detail` 分流：
 * 单击 → `start_dragging`，双击 → `internal_toggle_maximize`。它根本不依赖
 * `dblclick` 事件：自己挂 `onPointerDown → startDragging` 的话，每次按下都进
 * 原生拖动循环，第二下点击永远到不了 WebView，双击就"没反应"
 * （2026-10-15 实测踩过）。`internal_toggle_maximize` 需要单独的权限
 * `core:window:allow-internal-toggle-maximize`，见 capabilities/default.json。
 *
 * 关闭走 `win.close()` —— 它触发后端已有的 `CloseRequested` 处理（隐藏到托盘），
 * 与点系统关闭完全同一条路，不在前端另立「退出 / 隐藏」的分叉语义。
 *
 * 最大化按钮的图标靠监听 resized 事件刷新：没有「最大化状态变了」的事件可读，
 * 而任何进出最大化都会伴随一次 resize，去查一次 `isMaximized()` 即可。
 */
export function TitleBar() {
  const win = getCurrentWindow();
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    let alive = true;
    const sync = () => void win.isMaximized().then((m) => alive && setMaximized(m));
    sync();
    const un = win.listen("resized", sync);
    return () => {
      alive = false;
      void un.then((f) => f());
    };
  }, [win]);

  return (
    <div className="titlebar">
      <div className="titlebar-drag" data-tauri-drag-region />
      <div className="win-btns">
        <button className="win-btn" title="最小化" onClick={() => void win.minimize()}>
          <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden>
            <path d="M2.5 6.5h7" stroke="currentColor" strokeWidth="1.2" />
          </svg>
        </button>
        <button
          className="win-btn"
          title={maximized ? "还原" : "最大化"}
          onClick={() => void win.toggleMaximize()}
        >
          {maximized ? (
            <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden>
              <rect x="2" y="4" width="6" height="6" rx="1" fill="none" stroke="currentColor" strokeWidth="1.2" />
              <path d="M4 4V2.8A.8.8 0 0 1 4.8 2H9.2a.8.8 0 0 1 .8.8v4.4a.8.8 0 0 1-.8.8H10" fill="none" stroke="currentColor" strokeWidth="1.2" />
            </svg>
          ) : (
            <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden>
              <rect x="2.5" y="2.5" width="7" height="7" rx="1" fill="none" stroke="currentColor" strokeWidth="1.2" />
            </svg>
          )}
        </button>
        <button className="win-btn close" title="关闭（缩到托盘）" onClick={() => void win.close()}>
          <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden>
            <path d="M3 3l6 6M9 3l-6 6" stroke="currentColor" strokeWidth="1.2" />
          </svg>
        </button>
      </div>
    </div>
  );
}
