import { check, type DownloadEvent } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

export interface UpdateProgress {
  status:
    | "checking"
    | "available"
    | "downloading"
    | "installing"
    | "updated"
    | "no-update"
    | "error";
  message: string;
  /** 发现的新版本号（available 起有值，供调用方点亮/熄灭更新提醒） */
  version?: string;
  /** 已下载字节数（downloading 起有值，安装/重启阶段沿用最后一次） */
  downloaded?: number;
  /** 总字节数；undefined / 0 = 服务端没给 Content-Length，进度不可知 */
  total?: number;
}

/**
 * 可直接渲染的进度；**percent 为 null 表示「画不出进度条」**。
 *
 * 这与历史上那套「不确定态」的区别是：null 不再对应任何动画 —— 页面只报已下载
 * 字节数、不画条。更新过程里同时存在「一条穿梭的滑块」和「一条真进度条」是两种读法，
 * 混着出现只会让人以为进度条坏了，所以这里只认百分比这一种。
 */
export interface DownloadProgress {
  downloaded: number;
  total: number;
  /** 0–100；null = 总量未知（页面不画条，只报字节数） */
  percent: number | null;
}

/**
 * 把 UpdateProgress 收敛成能直接渲染的进度；不需要展示进度的阶段返回 null。
 *
 * 独立出来是因为「百分比怎么算、哪些阶段该有条」属于更新逻辑，不该散在页面里；
 * 页面只消费 { downloaded, total, percent } 三个数。
 *
 * 安装与重启阶段照样给 100%：下载刚跑完就被抽掉进度条，是整条更新流程里最刺眼的一跳
 * —— 条走到头停住、等应用重启，才像「一条从头走到尾的进度条」。
 */
export function downloadProgress(p: UpdateProgress | null): DownloadProgress | null {
  if (!p) return null;
  const downloaded = p.downloaded ?? 0;
  const total = p.total ?? 0;
  if (p.status === "installing" || p.status === "updated") {
    // 总量未知时仍给 null：没有分母就没有百分比，宁可只报字节数
    return { downloaded, total, percent: total > 0 ? 100 : null };
  }
  if (p.status !== "downloading") return null;
  return {
    downloaded,
    total,
    percent: total > 0 ? Math.min(100, Math.round((downloaded / total) * 100)) : null,
  };
}

/** 后台检查更新发现的新版本；seen = 用户已经看过（小红点随之熄灭） */
export type UpdateNotice = { version: string; seen: boolean } | null;

/**
 * 后台轮询结果 → 新的提醒状态。抽成纯函数是因为它有三条容易写错、又都影响体验的规则：
 * - 已是最新（version = null）→ 清空提醒；
 * - 同一版本再次查到 → 原样返回（连引用都不换），别把用户已经看过的提醒重新点亮；
 * - 冒出更新的版本 → 未读；但用户此刻就在设置页，则直接算已读，免得刚看完又被点一颗红点。
 */
export function nextUpdateNotice(
  prev: UpdateNotice,
  version: string | null,
  onSettingsPage: boolean
): UpdateNotice {
  if (!version) return null;
  if (prev?.version === version) return prev;
  return { version, seen: onSettingsPage };
}

/**
 * 只查询有没有新版本：不下载、不安装。
 *
 * 给后台定时轮询用（见 App.tsx 的更新提醒）。「已是最新」是正常结果，返回 null；
 * 真正的失败（网络不通、更新源配置不对）照旧抛给调用方，由调用方决定是否静默。
 */
export async function probeUpdate(): Promise<string | null> {
  const update = await check();
  if (!update) return null;
  const version = update.version;
  // Update 在 JS 侧是 Resource：只查不装就必须显式 close，
  // 否则每轮询一次就在 Rust 侧挂一个句柄，常年常驻的进程会一路攒下去。
  await update.close();
  return version;
}

/**
 * 检查并安装 GitHub Release 上的更新。
 * onProgress 用于驱动 UI 进度展示；安装完成后自动重启应用。
 */
export async function checkAndInstall(
  onProgress: (p: UpdateProgress) => void
): Promise<void> {
  onProgress({ status: "checking", message: "正在检查更新…" });
  let update;
  try {
    update = await check();
  } catch (e) {
    onProgress({
      status: "error",
      message: "检查更新失败：" + errMsg(e),
    });
    return;
  }

  if (!update) {
    onProgress({ status: "no-update", message: "已经是最新版本。" });
    return;
  }

  onProgress({
    status: "available",
    message: `发现新版本 ${update.version}`,
    version: update.version,
  });

  // 下载量必须跨事件累计，所以声明在 try 外面 —— 安装/重启阶段还要带上它，
  // 好让进度条停在满格（见 downloadProgress）。
  let downloaded = 0;
  // 总大小只在 Started 事件里给（Progress 事件只有 chunkLength），必须在这里记住它；
  // 若去 Progress 里取 contentLength，total 恒为 undefined → 百分比恒为 0 → 进度条永远是空条。
  let total = 0;

  try {
    const emit = () =>
      onProgress({
        status: "downloading",
        message: "正在下载更新…",
        downloaded,
        total,
      });
    emit();

    // switch 直接收窄 data 类型 —— 不要用 as 断言，
    // 正是「把 Progress 的数据断言成含 contentLength」才让这个 bug 躲过了 tsc。
    const onEvent = (event: DownloadEvent) => {
      switch (event.event) {
        case "Started":
          total = event.data.contentLength ?? 0;
          emit();
          break;
        case "Progress":
          downloaded += event.data.chunkLength;
          emit();
          break;
        case "Finished":
          onProgress({
            status: "installing",
            message: "正在安装更新…",
            downloaded,
            total,
          });
          break;
      }
    };
    await update.downloadAndInstall(onEvent);
  } catch (e) {
    onProgress({ status: "error", message: "更新失败：" + errMsg(e) });
    return;
  }

  onProgress({
    status: "updated",
    message: "更新完成，正在重启…",
    downloaded,
    total,
  });
  try {
    await relaunch();
  } catch (e) {
    onProgress({
      status: "error",
      message: "安装完成但重启失败，请手动重启：" + errMsg(e),
    });
  }
}

function errMsg(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}
