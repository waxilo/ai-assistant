import { useCallback, useEffect, useRef, useState } from "react";
import { discoverLocal, importAccounts, oauthStart, oauthPoll, openExternal } from "../api";
import type { Account, NewAccount } from "../types";
import { AccountCell, type Toast } from "../common";
import { Dialog } from "../components/Dialog";
import { IconUserPlus, IconRefresh } from "../components/Icons";

/**
 * 「添加账号」弹窗：打开即扫描本机登录态，列出找到的账号供勾选导入；
 * 另一条路是通过浏览器授权登录。
 *
 * 扫描只读不写：`discover_local` 已排除库里已有的账号，用户勾选后才调
 * `import_accounts` —— 不再有「点一下直接把所有本机账号塞进来」的动作。
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
  const [err, setErr] = useState("");
  const [found, setFound] = useState<Account[]>([]);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [scanning, setScanning] = useState(true);
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

  const scan = useCallback(async () => {
    setScanning(true);
    setErr("");
    try {
      const list = await discoverLocal();
      setFound(list);
      setPicked(new Set(list.map((a) => a.id)));
    } catch (e) {
      setFound([]);
      setPicked(new Set<string>());
      setErr("扫描失败：" + e);
    } finally {
      setScanning(false);
    }
  }, []);

  useEffect(() => {
    void scan();
  }, [scan]);

  const allPicked = found.length > 0 && picked.size === found.length;

  const toggle = (id: string) => {
    setPicked((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const importPicked = async () => {
    const list = found.filter((a) => picked.has(a.id));
    if (list.length === 0) return;
    setBusy(true);
    setErr("");
    try {
      const merged = await importAccounts(list);
      onImported(merged);
      notify({ kind: "ok", text: `已导入 ${list.length} 个本机账号` });
      onClose();
    } catch (e) {
      setErr("导入失败：" + e);
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
      const st = await oauthStart();
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
            user_id: p.uid ?? null,
            token: p.token,
            refresh_token: p.refresh_token ?? null,
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
      subtitle="导入本机已登录的账号，或通过浏览器登录。"
      onClose={onClose}
      footer={
        <>
          <button className="btn ghost" onClick={onClose}>
            {busy && waiting ? "取消" : "关闭"}
          </button>
          {tab === "scan" ? (
            <button
              className="btn primary"
              disabled={busy || scanning || picked.size === 0}
              onClick={() => void importPicked()}
            >
              {busy ? (
                <>
                  <IconRefresh size={15} className="spin" />
                  导入中…
                </>
              ) : picked.size > 0 ? (
                `导入 ${picked.size} 个账号`
              ) : (
                "导入所选"
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
            ["scan", "本机账号"],
            ["browser", "浏览器登录"],
          ] as [Tab, string][]
        ).map(([k, label]) => (
          <button
            key={k}
            className={tab === k ? "active" : ""}
            disabled={busy}
            onClick={() => {
              setTab(k);
              setErr("");
            }}
          >
            {label}
          </button>
        ))}
      </div>

      {/* 固定高度的内容槽：扫描中 / 列表 / 空态 / 浏览器登录 / 报错都在同一块里，
          弹窗不会因为「加载完成」「多出一行错误」而改变大小。 */}
      <div className="add-body">
        {tab === "scan" &&
          (scanning && found.length === 0 ? (
            <div className="discover-state">
              <IconRefresh size={20} className="spin" />
              <span>正在读取本机登录的账号…</span>
            </div>
          ) : found.length === 0 ? (
            <div className="discover-state">
              <IconUserPlus size={22} />
              <strong>{err ? "扫描失败" : "本机没有可导入的新账号"}</strong>
              <span>{err || "本机登录的账号都已导入，也可以切到「浏览器登录」添加。"}</span>
              <button className="btn small" disabled={scanning} onClick={() => void scan()}>
                {scanning ? "扫描中…" : "重新扫描"}
              </button>
            </div>
          ) : (
            <>
              <div className="discover-head">
                <label className="discover-all">
                  <input type="checkbox" checked={allPicked} onChange={() => setPicked(allPicked ? new Set<string>() : new Set(found.map((a) => a.id)))} />
                  全选
                </label>
                <span className="discover-count">发现 {found.length} 个本机登录账号</span>
                <button
                  className="btn small ghost"
                  disabled={scanning || busy}
                  onClick={() => void scan()}
                  title="重新读取本机登录态"
                >
                  <IconRefresh size={13} className={scanning ? "spin" : ""} />
                  {scanning ? "扫描中…" : "重新扫描"}
                </button>
              </div>
              <div className="discover-list">
                {found.map((a) => (
                  <label
                    key={a.id}
                    className={"discover-row" + (picked.has(a.id) ? " picked" : "")}
                  >
                    <input
                      type="checkbox"
                      checked={picked.has(a.id)}
                      onChange={() => toggle(a.id)}
                    />
                    <AccountCell name={a.name} phone={a.phone} />
                  </label>
                ))}
              </div>
            </>
          ))}

        {tab === "browser" && (
          <>
            <p className="hint">点「去登录」后在浏览器完成登录，本助手会自动导入。</p>
            {uri && (
              <div className="oauth-uri">
                <code title={uri}>{uri}</code>
                <button className="btn small" onClick={() => void openExternal(uri)}>
                  重新打开
                </button>
              </div>
            )}
            {waiting && <p className="hint">正在等待登录完成…</p>}
          </>
        )}

        {err && (tab === "browser" || found.length > 0) && (
          <p className="form-err">{err}</p>
        )}
      </div>
    </Dialog>
  );
}
