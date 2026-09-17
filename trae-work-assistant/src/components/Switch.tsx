/**
 * 统一开关控件：替代裸 checkbox，也替代「各页自己写一个 toggle」。
 *
 * 尺寸三档，语义各不相同，不要为了排版随意挑：
 * - `sm` 表格/紧凑场景；
 * - `md` 常规设置行（默认）；
 * - `lg` 整页唯一的主控件（智能接管的总开关就是它）。
 */
interface Props {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  size?: "sm" | "md" | "lg";
  /** 悬停提示：说明这个开关**会做什么**，而不是重复它的标签 */
  title?: string;
}

export default function Switch({
  checked,
  onChange,
  disabled,
  size = "md",
  title,
}: Props) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      title={title}
      className={`switch ${size}${checked ? " on" : ""}`}
      disabled={disabled}
      onClick={() => !disabled && onChange(!checked)}
    >
      <span className="knob" />
    </button>
  );
}
