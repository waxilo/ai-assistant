#!/usr/bin/env node
/**
 * 合并多个平台各自的 latest.json 成单一 latest.json。
 *
 * 为什么需要：GitHub Actions 里 mac / win 各跑一个 job，每个 job 生成的 latest.json
 * 只包含本平台的条目（platforms 只有自己的 target）。而 updater 端点指向的是
 * `releases/latest/download/latest.json` —— Release 上只能有**一个**该文件，
 * 谁后传谁赢，另一个平台就会永远查不到更新。这里把各平台的 platforms 并成一个。
 *
 * 用法：node scripts/merge-latest.mjs <a.json> [b.json ...]，产物写到当前目录 latest.json。
 */
import { readFileSync, writeFileSync } from "node:fs";

const files = process.argv.slice(2).filter((f) => f.endsWith(".json"));
if (files.length === 0) {
  console.error("用法: node scripts/merge-latest.mjs <latest.json> [更多...]");
  process.exit(1);
}

const merged = { platforms: {} };
for (const f of files) {
  let j;
  try {
    j = JSON.parse(readFileSync(f, "utf8"));
  } catch (e) {
    console.error(`读取 ${f} 失败: ${e.message}`);
    continue;
  }
  if (!j.platforms || typeof j.platforms !== "object") continue;
  for (const [key, val] of Object.entries(j.platforms)) {
    merged.platforms[key] = val;
  }
  for (const k of ["version", "notes", "pub_date"]) {
    if (merged[k] === undefined && j[k] !== undefined) merged[k] = j[k];
  }
}

if (Object.keys(merged.platforms).length === 0) {
  console.error("没有任何平台条目可合并");
  process.exit(1);
}

writeFileSync("latest.json", JSON.stringify(merged, null, 2) + "\n");
console.log(`已合并: ${Object.keys(merged.platforms).join(", ")}`);
