import { useEffect, useRef, useState } from "react";
import { discoverLocal, importAccounts, oauthStart, oauthPoll, openExternal } from "../api";
import type { Account, NewAccount } from "../types";
import type { Toast } from "../common";
import { Dialog } from "../components/Dialog";
import { IconUserPlus, IconRefresh } from "../components/Icons";

/**
 * 「添加账号」弹窗：两条路 —— 扫描本机登录态 / 浏览器授权登录。
 *
 * 弹窗外壳、Esc、遮罩关闭、滚动锁全部交给 `Dialog`（这里只写内容）。
 * 旧版是手写 `.overlay + .modal + stopPropagation`，于是它**按 Esc 关不掉**，
 * 也没有 dialog 语义。
 *
 * ⚠️ 浏览器登录是**轮询**：申请授权 → 打开系统浏览器 → 每 2 秒问一次回调是否拿到 token。
 * 轮询必须能在弹窗关闭时停下来（`cancelRef`），否则关掉弹窗后循环还在跑，
 * 拿到的 token 会被写进一个已经卸载的组件状态里（静默丢账号）。
 */
type Tab = "scan" | "browser";

interface Props {
  onClose: () => void;
  onImported: (accounts: Account[]) => void;
  notify: (t: Toast) => void;
}

export default function AddAccountModal({ onClose, onImported, notify }: Props) {
  const [tab, setTab] = useState<Tab>("scan");
  const [busy, setBusy] = useState(false);
  const [host, setHost] = useState("https://api.trae.cn");
  const [err, setErr] = useState("");
  /** 浏览器登录：已打开的授权页（给「重新打开」用） */
  const [uri, setUri] = useState("");
  const [waiting, setWaiting] = useState(false);
  const cancelRef = useRef(false);

  useEffect(() => {
    // 组件卸载 = 用户关掉了弹窗 ⇒ 停止轮询
    return () => {
      cancelRef.current = true;
    };
  }, []);

  const scan = async () => {
    setBusy(true);
    setErr("");
    try {
      const found = await discoverLocal();
      if (!found.length) {
        setErr("本机未发现新的未导入账号。");
        return;
      }
      const list = await importAccounts(found);
      onImported(list);
      notify({ kind: "ok", text: `已导入 ${found.length} 个本机账号` });
      onClose();
    } catch (e) {
      setErr("扫描失败：" + e);
    } finally {
      setBusy(false);
    }
  };

  /** 申请授权 → 打开系统浏览器 → 轮询回调 token → 导入 */
  const startBrowser = async () => {
    setBusy(true);
    setErr("");
    cancelRef.current = false;
    try {
      const st = await oauthStart(host.trim() || null);
      setUri(st.verification_uri);
      await openExternal(st.verification_uri);
      setWaiting(true);
      // 轮询直到拿到 token（done=true）、报错，或弹窗被关掉
      for (;;) {
        await new Promise((r) => window.setTimeout(r, 2000));
        if (cancelRef.current) return;
        const p = await oauthPoll(st.login_id);
        if (cancelRef.current) return;
        if (p.done) {
          if (p.error || !p.token) {
            setErr("登录失败：" + (p.error || "未获取到 token"));
            break;
          }
          // `id` / `created_at` 由后端补：前端编不出唯一 id，硬写空串会让
          // checkinOne / removeAccount / statuses 的按 id 匹配串号（见 types.ts）。
          // `name` 来自服务端 `ScreenName`（`GetUserInfo`），拿不到时才落占位名 ——
          // 后端 `profile.rs` 会在下次启动时按需回源把它换成真名。
          const acct: NewAccount = {
            name: p.nickname || p.phone || "浏览器登录账号",
            phone: p.phone ?? null,
            region: p.region ?? null,
            user_id: p.uid ?? null,
            token: p.token,
            refresh_token: p.refresh_token ?? null,
            host: p.host ?? null,
            expires_at: p.expires_at ?? null,
            refresh_expires_at: null,
            device_id: p.device_id ?? null,
            machine_id: p.machine_id ?? null,
          };
          const list = await importAccounts([acct]);
          onImported(list);
          notify({ kind: "ok", text: "已添加新账号" });
          onClose();
          return;
        }
      }
    } catch (e) {
      setErr("浏览器登录失败：" + e);
    } finally {
      if (!cancelRef.current) {
        setWaiting(false);
        setBusy(false);
      }
    }
  };

  return (
    <Dialog
      label="添加账号"
      title="添加账号"
      subtitle="把本机已登录的 TraeWork 账号收进来，或走浏览器授权新登录一个。"
      onClose={onClose}
      footer={
        <>
          <button className="btn ghost" onClick={onClose}>
            {busy && waiting ? "取消" : "关闭"}
          </button>
          {tab === "scan" ? (
            <button className="btn primary" disabled={busy} onClick={() => void scan()}>
              {busy ? (
                <>
                  <IconRefresh size={15} className="spin" />
                  扫描中…
                </>
              ) : (
                "扫描并导入"
              )}
            </button>
          ) : (
            <button className="btn primary" disabled={busy} onClick={() => void startBrowser()}>
              <IconUserPlus size={15} />
              {busy ? "等待登录…" : "去登录"}
            </button>
          )}
        </>
      }
    >
      <div className="seg">
        {(
          [
            ["scan", "扫描本机登录"],
            ["browser", "浏览器登录"],
          ] as [Tab, string][]
        ).map(([k, label]) => (
          <button
            key={k}
            className={tab === k ? "active" : ""}
            disabled={busy}
            onClick={() => setTab(k)}
          >
            {label}
          </button>
        ))}
      </div>

      {tab === "scan" && (
        <p className="hint">
          读取本机 TraeWork 桌面端已登录的账号（本地加密登录态）并导入。已经导入过的账号会按
          手机号或 token 合并，不会产生重复条目。
        </p>
      )}

      {tab === "browser" && (
        <>
          <p className="hint">
            点「去登录」后系统会打开浏览器授权页；在页面里完成登录，本助手会自动拿到 token 并导入。
          </p>
          <label>
            API Host（可选）
            <input
              value={host}
              onChange={(e) => setHost(e.target.value)}
              placeholder="https://api.trae.cn"
              disabled={busy}
            />
          </label>
          {uri && (
            <div className="oauth-uri">
              <code title={uri}>{uri}</code>
              <button className="btn small" onClick={() => void openExternal(uri)}>
                重新打开
              </button>
            </div>
          )}
          {waiting && <p className="hint">已打开浏览器，正在等待登录完成…</p>}
        </>
      )}

      {err && <p className="form-err">{err}</p>}
    </Dialog>
  );
}
