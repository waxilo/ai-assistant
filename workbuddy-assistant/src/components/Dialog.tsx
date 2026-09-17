import { useEffect, type ReactNode } from "react";
import { IconX } from "./Icons";

/**
 * 遮罩层 + 弹窗外壳。**全应用所有弹窗都必须走它。**
 *
 * # 为什么它长这样（三段式，且不接受任意 className）
 *
 * 抽出来的直接原因：弹窗在 3 个文件里出现了 5 次，每次都手写
 * `.modal-mask` + 点遮罩关闭 + `stopPropagation`。而只有「确认框」实现了
 * Esc 关闭，**另外 4 处按 Esc 关不掉**；5 处都没有 dialog 语义
 * （role / aria-modal），也没有背景滚动锁。行为散落在各使用点，必然漏掉几个。
 *
 * 第二轮（本轮）解决的是**长得不像一家**：外壳虽然只此一处，但每个弹窗自己拼
 * `<h2>`、自己贴按钮、自己写说明文字，于是出现了三种宽度、两种按钮对齐、
 * 四种说明块长相。所以现在外壳**替调用方决定解剖结构**：
 *
 * - `title` / `subtitle` → 头部（不滚，右上角一个统一的关闭按钮）
 * - `children` → 内容区（**唯一滚动区**，长内容不会把标题与按钮顶出视野）
 * - `tools` / `footer` → 底部（左「工具位」右「操作位」，主操作恒在最右）
 *
 * 而且**不再接受任意 className**：尺寸只有 `size` 三档，语义色只有 `tone`。
 * 想加新样式就得改这个文件 —— 这是防止「第 6 个弹窗又长出自己的样子」的唯一
 * 有效手段（前两轮都栽在「这次先用 className 顶一下」）。
 *
 * 调用方只负责**弹窗里放什么**，不负责它长什么样。
 */
export function Dialog({
  onClose,
  size = "md",
  tone = "plain",
  icon,
  title,
  subtitle,
  tools,
  footer,
  maskClassName,
  label,
  children,
}: {
  /** 关闭（点遮罩 / 按 Esc / 点右上角 ×） */
  onClose: () => void;
  /** 宽度档：sm 确认类 400 / md 默认 560 / lg 列表与表格 720 */
  size?: "sm" | "md" | "lg";
  /** 语义色：只影响头部图标座（危险操作再配 `.btn.danger`） */
  tone?: "plain" | "danger" | "warn" | "ok";
  /** 头部图标（可选）：给了它，弹窗就有个「一眼认出这是什么」的锚点 */
  icon?: ReactNode;
  /** 标题。**必填** —— 没有标题的弹窗，用户不知道自己在看什么 */
  title: ReactNode;
  /** 标题下的一句话说明（可选）。长段落请放进 `children` */
  subtitle?: ReactNode;
  /** 底部左侧：批量 / 次要操作（如「全部勾选」「刷新」） */
  tools?: ReactNode;
  /** 底部右侧：主操作（取消 / 保存…），**主操作放最后一个** */
  footer?: ReactNode;
  /** 遮罩层附加类名（层级覆盖，如 "confirm-mask"） */
  maskClassName?: string;
  /** 无障碍名称：通常传标题文字 */
  label?: string;
  children: ReactNode;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    // 背景滚动锁：本应用真正滚动的是内容区，不是 body，所以要给 body 挂个
    // 状态类让内容区停止滚动（见 ui.css 的 body.modal-open）。
    document.body.classList.add("modal-open");
    return () => {
      window.removeEventListener("keydown", onKey);
      document.body.classList.remove("modal-open");
    };
  }, [onClose]);

  return (
    <div
      className={"modal-mask" + (maskClassName ? " " + maskClassName : "")}
      onClick={onClose}
      role="presentation"
    >
      <div
        className={`modal modal-${size}${tone === "plain" ? "" : " tone-" + tone}`}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        onClick={(e) => e.stopPropagation()}
      >
        <header className="modal-head">
          {icon && <span className="modal-head-icon">{icon}</span>}
          <div className="modal-title">
            <h2>{title}</h2>
            {subtitle && <p className="modal-sub">{subtitle}</p>}
          </div>
          <button
            className="icon-btn modal-x"
            aria-label="关闭"
            title="关闭（Esc）"
            onClick={onClose}
          >
            <IconX size={16} />
          </button>
        </header>

        {/* 没有正文时不渲染 body：确认框允许只给标题，空 body 会在头尾之间
            留下一段 38px 的空白（看着像加载失败） */}
        {children ? <div className="modal-body">{children}</div> : null}

        {(tools || footer) && (
          <footer className="modal-foot">
            {tools && <div className="modal-foot-tools">{tools}</div>}
            {footer && <div className="modal-foot-actions">{footer}</div>}
          </footer>
        )}
      </div>
    </div>
  );
}
