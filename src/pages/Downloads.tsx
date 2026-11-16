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
    case "queued": return "排队中";
    case "downloading": return item.total ? `下载中 ${pct}% · ${fmtBytes(item.bytes)}` : "正在准备…";
    case "paused": return `已暂停 ${pct}%`;
    case "failed": return item.error ? `下载失败：${item.error}` : "下载失败";
  }
};

const active = (i: DownloadItem) => i.status === "downloading" || i.status === "queued";
const resumable = (i: DownloadItem) => i.status === "paused" || i.status === "failed";

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

type Group = { videoId: number; title: string; coverKey?: string; items: DownloadItem[]; latest: number };
type Confirm = { title: string; keys: string[]; label: string };

/** 下载列表：按剧分组（整季下载时一部剧几十集），下载页与设置页共用。 */
export function DownloadList({ compact = false }: { compact?: boolean }) {
  const { push, toast } = useStore();
  const { items, loaded, error: err } = useDownloads();
  const [pending, setPending] = useState(false);
  const [confirming, setConfirming] = useState<Confirm | null>(null);
  const [collapsed, setCollapsed] = useState<ReadonlySet<number>>(() => new Set());

  const groups = useMemo(() => {
    const map = new Map<number, Group>();
    for (const item of items) {
      const group = map.get(item.video_id) ?? { videoId: item.video_id, title: "", items: [], latest: 0 };
      group.items.push(item);
      group.title ||= item.title;
      if (!group.coverKey && item.has_cover) group.coverKey = item.key;
      group.latest = Math.max(group.latest, item.created_at);
      map.set(item.video_id, group);
    }
    const list = [...map.values()];
    list.forEach(g => g.items.sort((a, b) => a.ep - b.ep));
    return list.sort((a, b) => b.latest - a.latest);
  }, [items]);

  /** 对一组条目执行同一操作；逐个调用，单个失败不影响其余。 */
  const run = async (targets: DownloadItem[], action: "pause" | "resume" | "delete") => {
    if (pending || !targets.length) return;
    setPending(true);
    let failed = 0;
    try {
      if (action === "resume" && targets.length > 1) {
        await api.downloadSeason(targets[0].video_id, targets.map(i => i.ep));
      } else {
        for (const item of targets) {
          try {
            if (action === "pause") await api.downloadPause(item.key);
            else if (action === "resume") await api.downloadStart(item.video_id, item.ep);
            else await api.downloadDelete(item.key);
          } catch { failed++; }
        }
      }
      if (action === "delete") toast(failed ? `${failed} 项删除失败` : "已删除", failed ? "info" : "ok");
      else if (failed) toast(`${failed} 项操作失败`, "info");
    } catch (e) {
      toast(typeof e === "string" ? e : "操作失败", "info");
    } finally {
      setPending(false);
      if (action === "delete") setConfirming(null);
      void refreshDownloads();
    }
  };
  const byKeys = (keys: string[]) => items.filter(i => keys.includes(i.key));
  const toggle = (videoId: number) => setCollapsed(prev => {
    const next = new Set(prev);
    if (next.has(videoId)) next.delete(videoId); else next.add(videoId);
    return next;
  });

  return <div className={compact ? "p-3" : ""}>
      {err && <div role="alert" className="mb-3 rounded-(--r-sm) bg-s3 p-3 text-[13px] text-danger">{err}</div>}
      {loaded && !items.length && (
        <Empty icon={<IDownload size={28} />} title="还没有下载" sub="在播放页点「下载」或在选集里点「下载全集」，没网时也能在这里观看" />
      )}
      <ul className="flex flex-col gap-3">
        {groups.map(group => {
          const done = group.items.filter(i => i.status === "done").length;
          const bytes = group.items.reduce((n, i) => n + i.bytes, 0);
          const unfinished = group.items.filter(i => i.status !== "done");
          const progress = unfinished.length
            ? unfinished.reduce((n, i) => n + (i.total ? i.done / i.total : 0), 0) / unfinished.length * 100 : 100;
          const running = group.items.filter(active);
          const stopped = group.items.filter(resumable);
          const open = !collapsed.has(group.videoId);
          return <li key={group.videoId} className={`rounded-(--r-md) p-2.5 ${compact ? "bg-s2" : "bg-s1 elev-1"}`}>
            <div className="flex items-center gap-3">
              <Button onClick={() => toggle(group.videoId)} aria-expanded={open} aria-label={`${open ? "收起" : "展开"}「${group.title}」`}
                className={`relative ${compact ? "w-[54px] h-[72px]" : "w-[66px] h-[88px]"} shrink-0 overflow-hidden rounded-(--r-sm) bg-s3 pressable`}>
                {group.coverKey && <img src={offlineCover(group.coverKey)} alt="" className="w-full h-full object-cover" />}
              </Button>
              <div className="flex-1 min-w-0">
                <div className="text-[14px] font-semibold text-on truncate">{group.title || `剧集 ${group.videoId}`}</div>
                <div className="text-[12px] text-mut mt-0.5 tabular-nums">
                  已下载 {done}/{group.items.length} 集 · {fmtBytes(bytes)}
                  {running.length > 0 && ` · ${running.filter(i => i.status === "queued").length} 集排队`}
                </div>
                {unfinished.length > 0 && (
                  <div className="mt-1.5 h-1 rounded-full bg-s3 overflow-hidden" role="progressbar" aria-label="整部下载进度" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.floor(progress)}>
                    <div className="h-full bg-primary transition-[width]" style={{ width: `${progress}%` }} />
                  </div>
                )}
              </div>
              <div className="flex flex-col gap-1.5 shrink-0">
                {running.length > 0 && (
                  <Button aria-label="全部暂停" title="全部暂停" disabled={pending} onClick={() => void run(running, "pause")} className="ios-icon-button"><IPauseF size={16} /></Button>
                )}
                {running.length === 0 && stopped.length > 0 && (
                  <Button aria-label="全部继续" title="全部继续" disabled={pending} onClick={() => void run(stopped, "resume")} className="ios-icon-button"><IRefresh size={16} /></Button>
                )}
                <Button aria-label="删除整部" title="删除整部" disabled={pending}
                  onClick={() => setConfirming({ title: group.title, keys: group.items.map(i => i.key), label: `全部 ${group.items.length} 集` })}
                  className="ios-icon-button"><ITrash size={16} /></Button>
              </div>
            </div>
            {open && (
              <ul className="mt-2 flex flex-col">
                {group.items.map(item => {
                  const isDone = item.status === "done";
                  const pct = item.total ? (item.done / item.total) * 100 : 0;
                  return <li key={item.key} className="flex items-center gap-2 border-t border-on/5 py-1.5">
                    <Button disabled={!isDone} onClick={() => push({ kind: "offline", item: item.key })}
                      aria-label={isDone ? `播放第 ${item.ep} 集` : `第 ${item.ep} 集`}
                      className="flex-1 min-w-0 flex items-center gap-2 text-left pressable">
                      <span className={`w-7 h-7 shrink-0 rounded-full flex items-center justify-center ${isDone ? "bg-primary text-on-primary" : "bg-s3 text-mut"}`}>
                        {isDone ? <IPlayF size={11} /> : <IDownload size={12} />}
                      </span>
                      <span className="flex-1 min-w-0">
                        <span className="block text-[13px] text-on">第 {item.ep} 集</span>
                        <span className={`block text-[11px] break-words ${item.status === "failed" ? "text-danger" : "text-mut"}`}>{statusText(item)}</span>
                        {!isDone && item.status !== "queued" && (
                          <span className="mt-1 block h-[3px] rounded-full bg-s3 overflow-hidden">
                            <span className="block h-full bg-primary transition-[width]" style={{ width: `${pct}%` }} />
                          </span>
                        )}
                      </span>
                    </Button>
                    {active(item) && (
                      <Button aria-label={`暂停第 ${item.ep} 集`} disabled={pending} onClick={() => void run([item], "pause")} className="ios-icon-button"><IPauseF size={14} /></Button>
                    )}
                    {resumable(item) && (
                      <Button aria-label={`${item.status === "failed" ? "重试" : "继续"}第 ${item.ep} 集`} disabled={pending} onClick={() => void run([item], "resume")} className="ios-icon-button"><IRefresh size={14} /></Button>
                    )}
                    <Button aria-label={`删除第 ${item.ep} 集`} disabled={pending}
                      onClick={() => setConfirming({ title: item.title, keys: [item.key], label: `第 ${item.ep} 集` })}
                      className="ios-icon-button"><ITrash size={14} /></Button>
                  </li>;
                })}
              </ul>
            )}
          </li>;
        })}
      </ul>
      {items.length > 0 && (
        <p className="mt-4 text-center text-[12px] text-dim">同时最多下载 2 集，其余排队。手机端切到后台或退出应用会暂停，回来后点「继续」即可续传</p>
      )}
    <Modal open={!!confirming} onClose={() => { if (!pending) setConfirming(null); }} label="删除下载">
      <div className="p-5">
        <h2 className="text-[18px] font-bold">删除下载</h2>
        <p className="mt-2 text-[14px] text-mut">删除「{confirming?.title || "剧集"}」{confirming?.label}的离线文件？</p>
        <div className="mt-4 flex gap-3">
          <Button variant="secondary" disabled={pending} onClick={() => setConfirming(null)} className="flex-1">取消</Button>
          <Button variant="danger" disabled={pending} onClick={() => { if (confirming) void run(byKeys(confirming.keys), "delete"); }} className="flex-1 rounded-(--r-ctl) px-3">
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
