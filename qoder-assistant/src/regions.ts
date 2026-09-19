/**
 * 区域清单（国际版 / 国内版）在前端的**唯一入口**。
 *
 * 清单本身由后端的 `regions` 命令给出（`Region::ALL` 的投影）——「国际版 / 国内版」
 * 的中文名、以及「OpenAPI 在哪个域」这类事实只该有一处定义。这里只负责
 * **拉一次、缓存在模块里**：账号页、登录弹窗、接管页都要显示区域名，
 * 各拉各的会得到多份可能不同步的副本。
 *
 * 缓存挂在模块上而不是某个组件的 state 上：区域清单在一次运行里不会变，
 * 组件挂载 / 卸载不该让它重新问一遍后端。
 */
import { useEffect, useState } from "react";
import type { RegionOption } from "./types";
import { regions as fetchRegions } from "./api";

let cache: RegionOption[] | null = null;
let inflight: Promise<RegionOption[]> | null = null;

/**
 * 拉取区域清单：并发去重（多处在同一拍挂载只会打一次 IPC）+ 进程内缓存。
 *
 * 失败时**不**缓存任何东西 —— 下次挂载会重试，而不是把「一次失败」固化成空清单。
 */
export function loadRegions(): Promise<RegionOption[]> {
  if (cache) return Promise.resolve(cache);
  if (!inflight) {
    inflight = fetchRegions()
      .then((list) => {
        cache = list ?? [];
        return cache;
      })
      .finally(() => {
        inflight = null;
      });
  }
  return inflight;
}

/**
 * 区域清单的 hook。
 *
 * 拿不到时返回空数组，而不是「猜一份内置清单」：区域名说错了会让人以为账号被收到了
 * 另一个部署上去，而空数组只会让标签暂时不显示 —— 清单到货（一次 IPC）后组件自己重渲染。
 */
export function useRegions(): RegionOption[] {
  const [list, setList] = useState<RegionOption[]>(cache ?? []);
  useEffect(() => {
    if (cache) return;
    let alive = true;
    void loadRegions()
      .then((l) => {
        if (alive) setList(l);
      })
      .catch(() => {
        // 拉不到就保持空：不编一个区域名出来（理由见上）
      });
    return () => {
      alive = false;
    };
  }, []);
  return list;
}

/** 区域标识 → 中文名；清单里没有这个标识时返回 null（同样不编名字） */
export function regionLabel(
  list: RegionOption[],
  key?: string | null
): string | null {
  if (!key) return null;
  return list.find((r) => r.key === key)?.label ?? null;
}

/** 区域标识 → 一句话说明（域在哪），用于 tooltip 与副文案 */
export function regionHint(
  list: RegionOption[],
  key?: string | null
): string | null {
  if (!key) return null;
  return list.find((r) => r.key === key)?.hint ?? null;
}
