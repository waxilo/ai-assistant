/**
 * 界面字体方案。
 *
 * 字体偏好是**纯前端**的 UI 配置，不落 settings.json（那套由 Rust 后端托管，
 * 与功能逻辑强耦合），用 localStorage 持久化即可 —— 换了设备重装也不心痛。
 *
 * 应用方式是覆盖 tokens.css 里 `--font-ui` 这个 CSS 变量：body 与绝大多数控件
 * 都从它继承字体，一行 setProperty 就能整体换脸，无需逐条改样式。
 */

/** localStorage 键名：当前选中的字体方案 key */
const FONT_KEY = "ui.font";

export interface FontOption {
  /** 稳定标识，存进 localStorage；也是设置页下拉的 value */
  key: string;
  /** 下拉里显示的名字 */
  label: string;
  /** 系统默认：不覆盖变量，沿用 tokens.css 的字体栈 */
  family?: string;
}

/**
 * 常用中文字体栈，各带跨平台回退：
 * - Windows 用系统自带族名（SimSun/SimHei/KaiTi/FangSong/Microsoft YaHei）
 * - macOS 用对应的苹果名（Songti SC / Heiti SC / STKaiti / STFangsong / PingFang SC）
 * - 兜底由 `serif` / `sans-serif` 补足
 */
export const FONT_OPTIONS: FontOption[] = [
  { key: "default", label: "系统默认" },
  { key: "song", label: "宋体", family: '"SimSun", "Songti SC", serif' },
  { key: "hei", label: "黑体", family: '"SimHei", "Heiti SC", "Microsoft YaHei", sans-serif' },
  { key: "yahei", label: "微软雅黑", family: '"Microsoft YaHei", "PingFang SC", sans-serif' },
  { key: "kai", label: "楷体", family: '"KaiTi", "STKaiti", "Kaiti SC", serif' },
  { key: "fangsong", label: "仿宋", family: '"FangSong", "STFangsong", serif' },
];

const DEFAULT_KEY = "default";

function findOption(key: string): FontOption | undefined {
  return FONT_OPTIONS.find((o) => o.key === key);
}

/** 把某个方案套到页面上（不写 localStorage） */
export function applyFont(key: string, root = document.documentElement) {
  const family = findOption(key)?.family;
  if (family) root.style.setProperty("--font-ui", family);
  else root.style.removeProperty("--font-ui");
}

/** 读取已保存的方案；无记录时回落到系统默认 */
export function getFontKey(): string {
  const saved = localStorage.getItem(FONT_KEY);
  return saved != null && findOption(saved) ? saved : DEFAULT_KEY;
}

/** 保存 + 立即生效 */
export function setFontKey(key: string) {
  localStorage.setItem(FONT_KEY, key);
  applyFont(key);
}

/** 启动时调用：把上次保存的方案一路带到这次会话 */
export function initFont() {
  applyFont(getFontKey());
}