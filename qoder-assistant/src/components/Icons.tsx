import type { ReactNode } from "react";

/** 统一线性图标：currentColor 描边，1.75 线宽，视觉重量一致。 */
type IconProps = { size?: number; className?: string };

function Svg({
  size = 18,
  className,
  children,
}: IconProps & { children: ReactNode }) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.75}
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {children}
    </svg>
  );
}

export const IconCheck = (p: IconProps) => (
  <Svg {...p}>
    <path d="M20 6 9 17l-5-5" />
  </Svg>
);

/**
 * 品牌标记：字母 Q —— 侧栏标题左边那块蓝色方块里的白色字形。
 *
 * 它**不是**线性图标，所以不走上面的 `Svg`（那套是 1.75 的细描边）。
 * 这里的每个数都从 `scripts/make-icon.mjs` 的版面常量**折算**而来，换算比 = 12/270
 * （应用图标里环外径 270 ↦ viewBox 半径 12，即字形外接框正好填满 24 画布）：
 *
 * | make-icon.mjs | 算式 | 此处 |
 * |---|---|---|
 * | 环中径 222（=270−96/2） | 222 × 12/270 | `r = 9.85`（留 0.02 防贴边） |
 * | 环笔画 96 | 96 × 12/270 | `strokeWidth = 4.27` |
 * | 尾宽 86 | 86 × 12/270 | `strokeWidth = 3.82` |
 * | 尾起 70（半径） | 12 + 70×12/270×cos45° | `M14.2 14.2` |
 * | 尾终 312（半径） | 12 + 312×12/270×cos45° | `21.81 21.81` |
 *
 * ⚠️ **端帽必须是 `round`**：make-icon 里尾巴是「线段距离再减半宽」，也就是**胶囊**；
 * 用 `butt` 端帽尾巴会短掉 16%，读起来是「环上被啃了个缺口」而不是字母尾巴 ——
 * 而两者都叫「Q」，肉眼不比对根本看不出来。
 *
 * 尾巴刻意**穿过环的空心**：只贴着外沿伸出去会变成放大镜（试过，见 make-icon.mjs 的注释）。
 */
export const IconQ = ({ size = 18, className }: IconProps) => (
  <svg
    className={className}
    width={size}
    height={size}
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    strokeLinecap="round"
  >
    <circle cx="12" cy="12" r="9.85" strokeWidth={4.27} />
    <path d="M14.2 14.2 21.81 21.81" strokeWidth={3.82} />
  </svg>
);

export const IconSwap = (p: IconProps) => (
  <Svg {...p}>
    <path d="M17 3l4 4-4 4" />
    <path d="M21 7H7a4 4 0 0 0-4 4" />
    <path d="M7 21l-4-4 4-4" />
    <path d="M3 17h14a4 4 0 0 0 4-4" />
  </Svg>
);

export const IconActivity = (p: IconProps) => (
  <Svg {...p}>
    <path d="M22 12h-4l-3 9L9 3l-3 9H2" />
  </Svg>
);

export const IconList = (p: IconProps) => (
  <Svg {...p}>
    <path d="M8 6h13" />
    <path d="M8 12h13" />
    <path d="M8 18h13" />
    <path d="M3 6h.01" />
    <path d="M3 12h.01" />
    <path d="M3 18h.01" />
  </Svg>
);

export const IconGear = (p: IconProps) => (
  <Svg {...p}>
    <circle cx="12" cy="12" r="3" />
    <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
  </Svg>
);

export const IconUserPlus = (p: IconProps) => (
  <Svg {...p}>
    <path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" />
    <circle cx="9" cy="7" r="4" />
    <path d="M19 8v6M22 11h-6" />
  </Svg>
);

export const IconDownload = (p: IconProps) => (
  <Svg {...p}>
    <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
    <path d="M7 10l5 5 5-5" />
    <path d="M12 15V3" />
  </Svg>
);

/** 文字/字体：界面外观 */
export const IconType = (p: IconProps) => (
  <Svg {...p}>
    <polyline points="4 7 4 4 20 4 20 7" />
    <line x1="9" y1="20" x2="15" y2="20" />
    <line x1="12" y1="4" x2="12" y2="20" />
  </Svg>
);

/** 上传：把本机账号整体放到云端那一池上 */
export const IconUpload = (p: IconProps) => (
  <Svg {...p}>
    <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
    <path d="M17 8l-5-5-5 5" />
    <path d="M12 3v12" />
  </Svg>
);

/** 云端：凭证池的绑定状态 */
export const IconCloud = (p: IconProps) => (
  <Svg {...p}>
    <path d="M18 10h-1.26A8 8 0 1 0 9 20h9a5 5 0 0 0 0-10z" />
  </Svg>
);

/** 链接：绑定一个已有的池 uuid */
export const IconLink = (p: IconProps) => (
  <Svg {...p}>
    <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
    <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
  </Svg>
);

/** 断开链接：解除绑定（同时在云端删掉那一池） */
export const IconUnlink = (p: IconProps) => (
  <Svg {...p}>
    <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
    <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
    <path d="M3 3l18 18" />
  </Svg>
);

export const IconRefresh = (p: IconProps) => (
  <Svg {...p}>
    <path d="M21 12a9 9 0 1 1-2.64-6.36" />
    <path d="M21 3v6h-6" />
  </Svg>
);

export const IconTrash = (p: IconProps) => (
  <Svg {...p}>
    <path d="M3 6h18" />
    <path d="M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2" />
    <path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" />
  </Svg>
);

export const IconFile = (p: IconProps) => (
  <Svg {...p}>
    <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
    <path d="M14 2v6h6" />
    <path d="M9 13h6M9 17h6" />
  </Svg>
);

export const IconUser = (p: IconProps) => (
  <Svg {...p}>
    <path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2" />
    <circle cx="12" cy="7" r="4" />
  </Svg>
);

export const IconCalendar = (p: IconProps) => (
  <Svg {...p}>
    <rect x="3" y="4" width="18" height="18" rx="2" />
    <path d="M16 2v4M8 2v4M3 10h18" />
  </Svg>
);

export const IconClock = (p: IconProps) => (
  <Svg {...p}>
    <circle cx="12" cy="12" r="9" />
    <path d="M12 7v5l3 2" />
  </Svg>
);

export const IconPower = (p: IconProps) => (
  <Svg {...p}>
    <path d="M12 2v10" />
    <path d="M18.36 6.64a9 9 0 1 1-12.73 0" />
  </Svg>
);

export const IconShield = (p: IconProps) => (
  <Svg {...p}>
    <path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z" />
    <path d="M9 12l2 2 4-4" />
  </Svg>
);

export const IconBell = (p: IconProps) => (
  <Svg {...p}>
    <path d="M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9" />
    <path d="M13.73 21a2 2 0 0 1-3.46 0" />
  </Svg>
);

export const IconWrench = (p: IconProps) => (
  <Svg {...p}>
    <path d="M22 11.08V12a8.91 8.91 0 0 1-9.91 9 8.84 8.84 0 0 1-4-1l-3 3a1 1 0 0 1-1.41-1.41l3-3a8.84 8.84 0 0 1-1-4A8.91 8.91 0 0 1 12 3h.92a8 8 0 0 1 6 2.92l1.41-1.41a1 1 0 0 1 1.41 1.41l-1.41 1.41A8 8 0 0 1 22 11.08z" />
  </Svg>
);

export const IconPlug = (p: IconProps) => (
  <Svg {...p}>
    <path d="M9 2v6M15 2v6" />
    <path d="M6 8h12v3a6 6 0 0 1-12 0z" />
    <path d="M12 17v5" />
  </Svg>
);

export const IconAlertTriangle = (p: IconProps) => (
  <Svg {...p}>
    <path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z" />
    <path d="M12 9v4M12 17h.01" />
  </Svg>
);

export const IconCircleCheck = (p: IconProps) => (
  <Svg {...p}>
    <path d="M22 11.08V12a10 10 0 1 1-5.93-9.14" />
    <path d="M22 4 12 14.01l-3-3" />
  </Svg>
);

export const IconBolt = (p: IconProps) => (
  <Svg {...p}>
    <path d="M13 2 3 14h9l-1 8 10-12h-9l1-8z" />
  </Svg>
);

export const IconInfo = (p: IconProps) => (
  <Svg {...p}>
    <circle cx="12" cy="12" r="10" />
    <path d="M12 16v-4M12 8h.01" />
  </Svg>
);

export const IconX = (p: IconProps) => (
  <Svg {...p}>
    <path d="M18 6 6 18M6 6l12 12" />
  </Svg>
);
