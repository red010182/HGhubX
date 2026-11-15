import { useSyncExternalStore } from "react";
import { api, type DownloadItem } from "./api";

/* 离线下载状态：播放页进度条、下载页、设置页共用一份。
   有订阅者且有任务在下载时每秒拉一次列表；只读 Rust 内存里的清单，不访问上游。 */

type State = { items: DownloadItem[]; loaded: boolean; error: string };

let state: State = { items: [], loaded: false, error: "" };
const listeners = new Set<() => void>();
let timer: number | undefined;
let inflight: Promise<void> | null = null;

const emit = () => listeners.forEach(fn => fn());

function schedule() {
  if (timer !== undefined || !listeners.size) return;
  if (!state.items.some(i => i.status === "downloading" || i.status === "queued")) return;
  timer = window.setTimeout(() => { timer = undefined; void refreshDownloads(); }, 1000);
}

/** 立即刷新一次（开始/暂停/删除后调用），之后按需继续轮询。 */
export function refreshDownloads(): Promise<void> {
  if (inflight) return inflight;
  inflight = api.downloadList()
    .then(items => { state = { items, loaded: true, error: "" }; })
    .catch(e => { state = { ...state, loaded: true, error: typeof e === "string" ? e : "读取下载列表失败" }; })
    .finally(() => { inflight = null; emit(); schedule(); });
  return inflight;
}

function subscribe(fn: () => void) {
  listeners.add(fn);
  if (listeners.size === 1) void refreshDownloads();
  return () => {
    listeners.delete(fn);
    if (!listeners.size && timer !== undefined) { window.clearTimeout(timer); timer = undefined; }
  };
}

export function useDownloads(): State {
  return useSyncExternalStore(subscribe, () => state);
}

/** 一部剧已有下载记录的各集，按集数索引。 */
export function useVideoDownloads(videoId: number): Map<number, DownloadItem> {
  const { items } = useDownloads();
  const map = new Map<number, DownloadItem>();
  for (const item of items) if (item.video_id === videoId) map.set(item.ep, item);
  return map;
}

/** 某一集的下载状态（没下载过返回 undefined）。 */
export function useDownloadItem(videoId: number, ep: number): DownloadItem | undefined {
  const { items } = useDownloads();
  return items.find(i => i.video_id === videoId && i.ep === ep);
}
