import type { ReactNode } from "react";

/**
 * 设置类卡片内部的通用控件原子。
 *
 * 抽出来的原因：设置项在页面上出现过十几处，每一处都是「左侧标题 + 说明、右侧控件」
 * 的同一结构。各写一份 flex 容器必然漂移（行高、间距、分隔线归属各不一样）——
 * 旧版 SettingsPage 是在 `<div className="form">` 里裸排 label，连「一行」这个概念
 * 都没有，所以「启用定时签到」和「定时签到时刻」看起来是两个不同级别的设置。
 */

/** 一行设置：左侧标题 + 说明，右侧控件。 */
export function Row({
  title,
  desc,
  ctrl,
  /** 嵌套行：带浅色背景，视觉上从属于上一项开关 */
  sub,
  /** 展开区内部的行：不显示分隔线 */
  bare,
}: {
  title: string;
  desc?: string;
  ctrl: ReactNode;
  sub?: boolean;
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
