import { useState } from "react";
import { copyText, type Toast } from "../common";
import { Dialog } from "./Dialog";
import {
  IconAlertTriangle,
  IconCloud,
  IconInfo,
  IconLink,
} from "./Icons";

/**
 * 云端凭证池的两个「做完即走」弹窗：
 * - BrokerBindModal：把**这台机器**接到别处那一池上（粘贴 uuid）；
 * - PoolIssuedModal：上传成功后摊开那串 uuid（唯一需要被搬到别的机器上去的东西）。
 */

/**
 * 「绑定云端凭证池」：把**这台机器**接到别处那一池上。
 *
 * 跨机器共用走凭证池，不走「导出凭证文件」—— 那条路会把 refresh token 原样写进文件，
 * 于是几台机器各持一份副本，而官方续签是单链轮换：谁先签就把别人踢下线。
 *
 * ⚠️ 粒度是**整台机器**，不是单个账号：所以这个弹窗不收 `Account`。
 */
export function BrokerBindModal({
  onBind,
  onClose,
}: {
  onBind: (uuid: string) => Promise<void>;
  onClose: () => void;
}) {
  const [uuid, setUuid] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  const submit = async () => {
    const v = uuid.trim();
    if (!v) {
      setErr("请先粘贴 uuid");
      return;
    }
    setBusy(true);
    setErr("");
    try {
      await onBind(v);
      onClose();
    } catch (e) {
      // 失败就留在弹窗里显示原因：绑定这一步最常见的错法就是 uuid 抄漏了几位，
      // 关掉弹窗再让用户从别的地方找回来，等于把错误藏了
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      onClose={onClose}
      size="md"
      icon={<IconLink size={18} />}
      title="绑定云端凭证池"
      subtitle="把另一台机器上传的账号池接到本机"
      footer={
        <>
          <button className="btn ghost" onClick={onClose} disabled={busy}>
            取消
          </button>
          <button
            className="btn primary"
            onClick={() => void submit()}
            disabled={busy}
          >
            {busy ? "绑定中…" : "绑定"}
          </button>
        </>
      }
    >
      <label>
        池 uuid
        <input
          value={uuid}
          autoFocus
          spellCheck={false}
          placeholder="xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
          onChange={(e) => {
            setUuid(e.target.value);
            setErr("");
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") void submit();
          }}
        />
      </label>
      {/* 说明与错误共用同一个固定高度槽：出错只在原地换字，弹窗高度不变 */}
      <p className={`note broker-note${err ? " danger" : ""}`}>
        {err ? <IconAlertTriangle size={14} /> : <IconInfo size={14} />}
        <span>{err || "绑定后两边账号合并，本机已有的账号不会丢失。"}</span>
      </p>
    </Dialog>
  );
}

/**
 * 上传成功后摊开那串 uuid。
 *
 * **用弹窗而不是 toast**：uuid 是唯一需要被「搬到别的机器」上去的东西 ——
 * 一条 3 秒就消失的提示等于没给。这里还要能一键复制，并把「它等于密码」
 * 这件事当面说清楚，而不是塞进一句会自动消失的提示里。
 */
export function PoolIssuedModal({
  uuid,
  message,
  onClose,
  onToast,
}: {
  uuid: string;
  message: string;
  onClose: () => void;
  onToast: (t: Toast) => void;
}) {
  const [copied, setCopied] = useState(false);

  const copy = async () => {
    const ok = await copyText(uuid);
    setCopied(ok);
    onToast(
      ok
        ? { kind: "ok", text: "uuid 已复制到剪贴板" }
        : { kind: "err", text: "复制失败，请手动选中下面这串" }
    );
  };

  return (
    <Dialog
      onClose={onClose}
      size="md"
      tone="ok"
      icon={<IconCloud size={18} />}
      title="这一池的 uuid"
      subtitle="复制到其他机器的「绑定云端凭证池」中使用"
      tools={
        <button className="btn small" onClick={() => void copy()}>
          {copied ? "已复制" : "复制 uuid"}
        </button>
      }
      footer={
        <button className="btn primary" onClick={onClose}>
          我已保存好
        </button>
      }
    >
      <p className="note">
        <IconInfo size={14} />
        <span>{message}</span>
      </p>
      {/* 点一下即全选：即使剪贴板两条路都不通，用户也能手动 Ctrl+C */}
      <code className="pool-uuid">{uuid}</code>
      <p className="note warn">
        <IconAlertTriangle size={14} />
        <span>这串 uuid 等同于账号密码，请妥善保存，不要外传。</span>
      </p>
    </Dialog>
  );
}
