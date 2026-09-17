import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

export interface UpdateProgress {
  status: "checking" | "available" | "downloading" | "installing" | "updated" | "no-update" | "error";
  message: string;
  /** 已下载字节数 */
  downloaded?: number;
  /** 总字节数；服务端没给 `Content-Length` 时为 undefined（此时只能显示已下载量） */
  total?: number;
}

/** 下载进度百分比（0–100）；总量未知时返回 null。 */
export function percentOf(p: UpdateProgress | null): number | null {
  if (!p || !p.total || p.total <= 0) return null;
  return Math.min(100, Math.round(((p.downloaded ?? 0) / p.total) * 100));
}

/**
 * 检查并安装 GitHub Release 上的更新。
 *
 * 进度来自 updater 插件的下载事件：`Started` 带 `contentLength`（总大小），
 * `Progress` 每次带一段 `chunkLength`。**必须自己累加 `chunkLength`** ——
 * 插件不给累计值，早期实现只把事件当成「还在下载」的信号，所以界面上只能显示一句话、
 * 没有进度可看。
 *
 * `onProgress` 用于驱动 UI；安装完成后自动重启应用。
 */
export async function checkAndInstall(
  onProgress: (p: UpdateProgress) => void
): Promise<void> {
  onProgress({ status: "checking", message: "正在检查更新…" });
  let update;
  try {
    update = await check();
  } catch (e) {
    onProgress({ status: "error", message: "检查更新失败：" + errMsg(e) });
    return;
  }

  if (!update) {
    onProgress({ status: "no-update", message: "已经是最新版本。" });
    return;
  }

  onProgress({
    status: "available",
    message: `发现新版本 ${update.version}，开始下载…`,
    downloaded: 0,
  });

  let downloaded = 0;
  let total: number | undefined;
  try {
    await update.downloadAndInstall((event) => {
      switch (event.event) {
        case "Started":
          total = event.data.contentLength;
          onProgress({
            status: "downloading",
            message: `正在下载 v${update.version}…`,
            downloaded,
            total,
          });
          break;
        case "Progress":
          downloaded += event.data.chunkLength;
          onProgress({
            status: "downloading",
            message: `正在下载 v${update.version}…`,
            downloaded,
            total,
          });
          break;
        case "Finished":
          onProgress({
            status: "installing",
            message: "下载完成，正在安装…",
            downloaded,
            total,
          });
          break;
      }
    });
    onProgress({
      status: "installing",
      message: "正在安装更新…",
      downloaded,
      total,
    });
  } catch (e) {
    onProgress({ status: "error", message: "更新失败：" + errMsg(e) });
    return;
  }

  onProgress({ status: "updated", message: "更新完成，正在重启…" });
  try {
    await relaunch();
  } catch (e) {
    onProgress({
      status: "error",
      message: "安装完成但重启失败，请手动重启：" + errMsg(e),
    });
  }
}

/** 字节数 → 人读的 MB（保留一位小数） */
export function mb(bytes: number): string {
  return (bytes / 1024 / 1024).toFixed(1);
}

function errMsg(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}
