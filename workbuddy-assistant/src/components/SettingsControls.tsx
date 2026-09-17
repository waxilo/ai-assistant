import type { ReactNode } from "react";

/**
 * 设置类卡片内部的通用控件原子。
 *
 * 抽出来是因为「网络急救」从独立页并入设置页后要复用同一套行/开关样式——
 * 两处各写一份必然漂移。
 */

/** 开关控件：复用全局 .switch 样式（与智能接管页一致） */
export function Toggle({
  checked,
  disabled,
  onChange,
  title,
}: {
  checked: boolean;
  disabled?: boolean;
  onChange: (v: boolean) => void;
  title?: string;
}) {
  return (
    <label className="switch" title={title}>
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
      />
      <span className="track">
        <span className="thumb" />
      </span>
    </label>
  );
}

/** 一行设置：左侧标题 + 说明，右侧控件 */
export function Row({
  title,
  desc,
  ctrl,
  sub,
  bare,
}: {
  title: string;
  desc?: string;
  ctrl: ReactNode;
  /** 嵌套行：带浅色背景，视觉上从属于上一项开关 */
  sub?: boolean;
  /** 展开区内部的行：不显示分隔线 */
  bare?: boolean;
}) {
  return (
    <div className={`set-row${sub ? " sub" : ""}${bare ? " in-expand" : ""}`}>
      <div className="set-row-main">
        <div className="set-row-title">{title}</div>
        {desc && <div className="set-row-desc">{desc}</div>}
      </div>
      <div className="set-row-ctrl">{ctrl}</div>
    </div>
  );
}
