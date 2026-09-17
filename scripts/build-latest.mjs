#!/usr/bin/env node
// 生成 Tauri v2 updater 所需的 latest.json（v2 的 tauri build 只产出 .sig，不再自动生成 manifest）。
//
// 用法：node scripts/build-latest.mjs <version> <releaseTag> <dir> <repo>
//   例：node scripts/build-latest.mjs 0.1.11 traework-latest dist waxilo/ai-assistant
//
// 扫描 <dir> 下各平台的 `.sig` 文件，按文件名推断平台并填进 `platforms`，
// 产物 URL 指向 `<repo>` 固定 tag Release 上的同名文件，输出 <dir>/latest.json。

import fs from "node:fs";
import path from "node:path";

const [version, tag, dir, repo] = process.argv.slice(2);
if (!version || !tag || !dir || !repo) {
  console.error("用法: node scripts/build-latest.mjs <version> <releaseTag> <dir> <repo>");
  process.exit(1);
}

// 每种 .sig 对应的平台 key 与产物文件名（去掉 .sig 后缀）
const RULES = [
  { sig: ".app.tar.gz.sig", keys: ["darwin-x86_64", "darwin-aarch64"] },
  { sig: ".exe.sig", keys: ["windows-x86_64"] },
  { sig: ".msi.sig", keys: ["windows-x86_64"] },
];

const platforms = {};
for (const f of fs.readdirSync(dir)) {
  const full = path.join(dir, f);
  if (!fs.statSync(full).isFile()) continue;

  const rule = RULES.find((r) => f.endsWith(r.sig));
  if (!rule) continue;

  const artifact = f.slice(0, -rule.sig.length + rule.sig.length - ".sig".length);
  const signature = fs.readFileSync(full, "utf8").trim();
  if (!signature) {
    console.error(`签名文件为空: ${f}`);
    process.exit(1);
  }
  const url = `https://github.com/${repo}/releases/download/${tag}/${encodeURIComponent(artifact)}`;
  for (const key of rule.keys) {
    platforms[key] = { signature, url };
  }
}

const keys = Object.keys(platforms);
if (keys.length === 0) {
  console.error(`在 ${dir} 下没找到任何 *.sig 文件，无法生成 latest.json`);
  process.exit(1);
}

const manifest = {
  version,
  notes: "",
  pub_date: new Date().toISOString(),
  platforms,
};

const out = path.join(dir, "latest.json");
fs.writeFileSync(out, JSON.stringify(manifest, null, 2) + "\n");
console.log(`已生成 ${out}，包含平台: ${keys.join(", ")}`);
