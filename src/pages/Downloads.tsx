import { Button } from "../components/ios/controls";
import { useMemo, useState } from "react";
import { refreshDownloads, useDownloads } from "../downloadStore";
import { api, fmtBytes, offlineCover, offlineSrc, type DownloadItem } from "../api";
import { useStore } from "../store";
import { Empty, Modal } from "../components/ui";
import Player from "../components/Player";
import { IBack, IDownload, IPauseF, IPlayF, IRefresh, ITrash } from "../icons";

export const statusText = (item: DownloadItem) => {
  const pct = item.total ? Math.floor((item.done / item.total) * 100) : 0;
  switch (item.status) {
    case "done": return `已完成 · ${fmtBytes(item.bytes)}`;
    case "downloading": return item.total ? `下载中 ${pct}% · ${fmtBytes(item.bytes)}` : "正在准备…";
    case "paused": return `已暂停 ${pct}%`;
    case "failed": return item.error ? `下载失败：${item.error}` : "下载失败";
  }
};

export default function DownloadsPage({ onBack }: { onBack: () => void }) {
  const { items } = useDownloads();
  const total = useMemo(() => items.reduce((n, i) => n + i.bytes, 0), [items]);
  return <div className="ios-page absolute inset-0 bg-bg flex flex-col">
    <div className="shrink-0 bg-s1" style={{ paddingTop: "var(--sat, 0px)" }}>
      <div className="px-3 pt-2 pb-3 flex items-center gap-2.5">
        <Button aria-label="返回" onClick={onBack} className="ios-icon-button"><IBack size={20} /></Button>
        <h1 className="flex-1 min-w-0 text-[16px] font-bold text-on truncate">离线下载</h1>
        {total > 0 && <span className="text-[12px] text-mut tabular-nums">共 {fmtBytes(total)}</span>}
      </div>
    </div>
    <div className="flex-1 scroll-y px-4 pt-3 pb-8"><DownloadList /></div>
  </div>;
}

/** 下载列表：下载页与设置页共用（进度、暂停/继续、删除、离线播放）。 */
export function DownloadList({ compact = false }: { compact?: boolean }) {
  const { push, toast } = useStore();
  const { items, loaded, error: err } = useDownloads();
  const [pending, setPending] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<DownloadItem | null>(null);
  const reload = refreshDownloads;

  const act = async (item: DownloadItem, action: "pause" | "resume" | "delete") => {
    if (pending) return;
    setPending(item.key);
    try {
      if (action === "pause") await api.downloadPause(item.key);
      else if (action === "resume") await api.downloadStart(item.video_id, item.ep);
      else { await api.downloadDelete(item.key); toast("已删除", "ok"); }
    } catch (e) {
      toast(typeof e === "string" ? e : "操作失败", "info");
    } finally {
      setPending(null);
      if (action === "delete") setConfirming(null);
      void reload();
    }
  };

  return <div className={compact ? "p-3" : ""}>
      {err && <div role="alert" className="mb-3 rounded-(--r-sm) bg-s3 p-3 text-[13px] text-danger">{err}</div>}
      {loaded && !items.length && (
        <Empty icon={<IDownload size={28} />} title="还没有下载" sub="在播放页点「下载」，没网时也能在这里观看" />
      )}
      <ul className="flex flex-col gap-2.5">
        {items.map(item => {
          const pct = item.total ? (item.done / item.total) * 100 : 0;
          const done = item.status === "done";
          return <li key={item.key} className={`flex items-center gap-3 rounded-(--r-md) p-2.5 ${compact ? "bg-s2" : "bg-s1 elev-1"}`}>
            <Button disabled={!done} aria-label={done ? `播放 ${item.title} 第 ${item.ep} 集` : undefined}
              onClick={() => push({ kind: "offline", item: item.key })}
              className={`relative ${compact ? "w-[54px] h-[72px]" : "w-[72px] h-[96px]"} shrink-0 overflow-hidden rounded-(--r-sm) bg-s3 pressable`}>
              {item.has_cover && <img src={offlineCover(item.key)} alt="" className="w-full h-full object-cover" />}
              {done && <span className="absolute inset-0 flex items-center justify-center bg-black/25"><IPlayF size={22} className="text-white" /></span>}
            </Button>
            <div className="flex-1 min-w-0">
              <div className="text-[14px] font-semibold text-on truncate">{item.title || `剧集 ${item.video_id}`}</div>
              <div className="text-[12px] text-mut mt-0.5">第 {item.ep} 集</div>
              <div className={`text-[12px] mt-1 break-words ${item.status === "failed" ? "text-danger" : "text-mut"}`}>{statusText(item)}</div>
              {!done && (
                <div className="mt-1.5 h-1 rounded-full bg-s3 overflow-hidden" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.floor(pct)}>
                  <div className="h-full bg-primary transition-[width]" style={{ width: `${pct}%` }} />
                </div>
              )}
            </div>
            <div className="flex flex-col gap-1.5 shrink-0">
              {item.status === "downloading" && (
                <Button aria-label="暂停" disabled={pending === item.key} onClick={() => void act(item, "pause")} className="ios-icon-button"><IPauseF size={16} /></Button>
              )}
              {(item.status === "paused" || item.status === "failed") && (
                <Button aria-label={item.status === "failed" ? "重试" : "继续"} disabled={pending === item.key} onClick={() => void act(item, "resume")} className="ios-icon-button"><IRefresh size={16} /></Button>
              )}
              <Button aria-label="删除" disabled={pending === item.key} onClick={() => setConfirming(item)} className="ios-icon-button"><ITrash size={16} /></Button>
            </div>
          </li>;
        })}
      </ul>
      {items.length > 0 && (
        <p className="mt-4 text-center text-[12px] text-dim">手机端切到后台或退出应用会暂停下载，回来后点「继续」即可续传</p>
      )}
    <Modal open={!!confirming} onClose={() => { if (!pending) setConfirming(null); }} label="删除下载">
      <div className="p-5">
        <h2 className="text-[18px] font-bold">删除下载</h2>
        <p className="mt-2 text-[14px] text-mut">删除「{confirming?.title || "剧集"}」第 {confirming?.ep} 集的离线文件？</p>
        <div className="mt-4 flex gap-3">
          <Button variant="secondary" disabled={!!pending} onClick={() => setConfirming(null)} className="flex-1">取消</Button>
          <Button variant="danger" disabled={!!pending} onClick={() => { if (confirming) void act(confirming, "delete"); }} className="flex-1 rounded-(--r-ctl) px-3">
            {pending ? "正在删除…" : "删除"}
          </Button>
        </div>
      </div>
    </Modal>
  </div>;
}

/** 离线播放：只读本地文件；同一部剧已下载的其他集可直接切换。 */
export function OfflinePlayer({ itemKey, onBack, active }: { itemKey: string; onBack: () => void; active: boolean }) {
  const { items, loaded } = useDownloads();
  const [key, setKey] = useState(itemKey);
  const [muted, setMuted] = useState(false);
  const current = items.find(i => i.key === key);
  const siblings = useMemo(
    () => items.filter(i => i.status === "done" && current && i.video_id === current.video_id).sort((a, b) => a.ep - b.ep),
    [items, current],
  );
  const next = siblings.find(i => current && i.ep > current.ep);

  return <div className="ios-page absolute inset-0 bg-black flex flex-col">
    <div className="shrink-0" style={{ paddingTop: "var(--sat, 0px)" }}>
      <div className="px-3 pt-2 pb-2 flex items-center gap-2.5 text-white">
        <Button aria-label="返回" onClick={onBack} className="ios-icon-button"><IBack size={20} /></Button>
        <h1 className="flex-1 min-w-0 text-[15px] font-bold truncate">
          {current ? `${current.title || `剧集 ${current.video_id}`} · 第 ${current.ep} 集` : "离线播放"}
        </h1>
      </div>
    </div>
    <div className="relative flex-1 min-h-0">
      {current?.status === "done" ? (
        <Player key={key} src={offlineSrc(key)} poster={current.has_cover ? offlineCover(key) : undefined} autoPlay active={active}
          muted={muted} onToggleMute={() => setMuted(m => !m)} className="w-full h-full"
          diagnosticContext={{ video_id: current.video_id, ep: current.ep, offline: true }}
          onEnded={() => { if (next) setKey(next.key); }} />
      ) : loaded ? (
        <div className="absolute inset-0 flex items-center justify-center text-[14px] text-white/70">这一集还没下载完成</div>
      ) : null}
    </div>
    {siblings.length > 1 && (
      <div className="shrink-0 px-4 pt-3 flex gap-2 overflow-x-auto" style={{ paddingBottom: "calc(var(--sab, 0px) + 12px)" }}>
        {siblings.map(i => (
          <Button key={i.key} aria-current={i.key === key ? "true" : undefined} onClick={() => setKey(i.key)}
            className={`h-9 min-w-12 px-3 rounded-(--r-sm) text-[13px] font-semibold shrink-0 pressable ${i.key === key ? "bg-primary text-on-primary" : "bg-white/15 text-white/85"}`}>
            {i.ep}
          </Button>
        ))}
      </div>
    )}
  </div>;
}
