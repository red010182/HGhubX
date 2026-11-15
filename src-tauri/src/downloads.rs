//! 离线下载：把一集 HLS 的播放列表、AES key、分片原样落盘，离线时经本地协议回放。
//!
//! 目录：`<app_data>/downloads/<视频ID>_<集数>/`
//! - `index.m3u8`：改写成相对文件名的本地播放列表（全部下载完才写，存在即代表可播）
//! - `k00000.key` / `m00001.mp4` / `s00002.ts`：key、init 分片、媒体分片，按清单顺序编号
//! - `cover`：已解密的封面
//!
//! 分片保持加密状态，由 hls.js / 原生 HLS 用落盘的 key 解密；回放地址
//! `${LOCAL}/dl/<key>/index.m3u8` 里全是相对路径，所以三种本地协议基址都不需要特殊处理。
//! 分片地址带时效签名：遇到 4xx 会重新取一次播放地址，按序号续传（不按 URL 匹配）。

use crate::media::{absolutize, map_uri_attributes};
use crate::net::fetch_bytes;
use once_cell::sync::{Lazy, OnceCell};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const CONCURRENCY: usize = 4;
const MAX_REFRESH: u32 = 3;
const MANIFEST: &str = "downloads.json";
const CANCELLED: &str = "已暂停";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Downloading,
    Paused,
    Failed,
    Done,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub key: String,
    pub video_id: i64,
    pub ep: u32,
    pub title: String,
    #[serde(default)]
    pub cover_url: String,
    #[serde(default)]
    pub has_cover: bool,
    pub status: Status,
    #[serde(default)]
    pub done: usize,
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub created_at: u64,
}

static ROOT: OnceCell<PathBuf> = OnceCell::new();
static ITEMS: Lazy<Mutex<Vec<Item>>> = Lazy::new(|| Mutex::new(Vec::new()));
static TASKS: Lazy<Mutex<HashMap<String, Arc<AtomicBool>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn task_key(id: i64, ep: u32) -> String {
    format!("{id}_{ep}")
}

/// `<视频ID>_<集数>`，只含数字与一个下划线。
pub fn valid_key(key: &str) -> bool {
    key.split_once('_').is_some_and(|(a, b)| {
        !a.is_empty() && !b.is_empty() && a.len() <= 12 && b.len() <= 6
            && a.bytes().all(|c| c.is_ascii_digit())
            && b.bytes().all(|c| c.is_ascii_digit())
    })
}

/// 可对外提供的文件名：`index.m3u8` / `video.m3u8` / `audio.m3u8`、`cover`，或 `<k|m|s|a><5 位序号>.<1-4 位小写扩展名>`。
fn valid_file(name: &str) -> bool {
    if matches!(name, "index.m3u8" | "video.m3u8" | "audio.m3u8" | "cover") {
        return true;
    }
    let b = name.as_bytes();
    let Some((stem, ext)) = name.split_once('.') else { return false };
    b.len() >= 8
        && matches!(b[0], b'k' | b'm' | b's' | b'a')
        && stem.len() >= 6
        && stem[1..].bytes().all(|c| c.is_ascii_digit())
        && (1..=4).contains(&ext.len())
        && ext.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn root() -> Result<&'static PathBuf, String> {
    ROOT.get().ok_or_else(|| "下载目录未初始化".to_string())
}

/// 启动时绑定目录并读回清单；上次进程退出时仍在下载的条目改为「已暂停」。
pub fn bind(app_data: &Path) {
    let dir = app_data.join("downloads");
    let _ = std::fs::create_dir_all(&dir);
    let mut items: Vec<Item> = std::fs::read(dir.join(MANIFEST))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    items.retain(|i| valid_key(&i.key));
    for item in items.iter_mut() {
        if item.status == Status::Downloading {
            item.status = Status::Paused;
        }
        if item.status == Status::Done && !dir.join(&item.key).join("index.m3u8").is_file() {
            item.status = Status::Failed;
            item.error = Some("本地文件已丢失，请重新下载".into());
        }
    }
    *ITEMS.lock().unwrap() = items;
    let _ = ROOT.set(dir);
}

fn save() {
    let Ok(dir) = root() else { return };
    let json = match serde_json::to_vec_pretty(&*ITEMS.lock().unwrap()) {
        Ok(j) => j,
        Err(_) => return,
    };
    let tmp = dir.join(format!("{MANIFEST}.tmp"));
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(MANIFEST));
    }
}

fn update(key: &str, persist: bool, f: impl FnOnce(&mut Item)) {
    {
        let mut items = ITEMS.lock().unwrap();
        match items.iter_mut().find(|i| i.key == key) {
            Some(item) => f(item),
            None => return,
        }
    }
    if persist {
        save();
    }
}

pub fn list() -> Vec<Item> {
    let mut items = ITEMS.lock().unwrap().clone();
    items.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.ep.cmp(&b.ep)));
    items
}

/* ───────────────────────── 播放列表本地化 ───────────────────────── */

#[derive(Debug, PartialEq)]
pub(crate) struct Plan {
    /// (上游绝对地址, 本地文件名)，按清单出现顺序，同一地址只下一次（BYTERANGE 复用）
    pub files: Vec<(String, String)>,
    /// 要写出的本地播放列表 (文件名, 内容)，URI 全是相对文件名。
    /// `index.m3u8` 总是最后一个：它存在就代表这一集下载完整、可以播放。
    pub outputs: Vec<(String, String)>,
}

impl Plan {
    /// 不含任何 URL 的清单指纹，用于判断续传时结构是否变化。
    fn signature(&self) -> String {
        self.outputs.iter().map(|(name, text)| format!("## {name}\n{text}")).collect()
    }
}

fn ext_of(url: &str, fallback: &'static str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or("");
    match last.rsplit_once('.') {
        Some((_, ext))
            if (1..=4).contains(&ext.len()) && ext.bytes().all(|c| c.is_ascii_alphanumeric()) =>
        {
            ext.to_ascii_lowercase()
        }
        _ => fallback.to_string(),
    }
}

/// 视频、音轨共用一张表：文件名全局唯一，同一个 key 只下一次。
#[derive(Default)]
struct Namer {
    files: Vec<(String, String)>,
    names: HashMap<String, String>,
}

impl Namer {
    fn intern(&mut self, remote: String, prefix: char, fallback: &'static str) -> String {
        if let Some(name) = self.names.get(&remote) {
            return name.clone();
        }
        let name = format!("{prefix}{:05}.{}", self.files.len(), ext_of(&remote, fallback));
        self.names.insert(remote.clone(), name.clone());
        self.files.push((remote, name.clone()));
        name
    }
}

/// 取属性值（去掉引号）。
fn attr(line: &str, key: &str) -> Option<String> {
    let (_, attributes) = crate::media::hls_attributes(line)?;
    attributes.into_iter().find_map(|a| {
        let (k, v) = a.split_once('=')?;
        (k.trim().eq_ignore_ascii_case(key)).then(|| v.trim().trim_matches('"').to_string())
    })
}

/// 按属性名过滤后重组标签行。
fn without_attrs(line: &str, drop: &[&str]) -> String {
    let Some((tag, attributes)) = crate::media::hls_attributes(line) else { return line.to_string() };
    let kept: Vec<&str> = attributes
        .into_iter()
        .filter(|a| {
            let key = a.split('=').next().unwrap_or("").trim().to_ascii_uppercase();
            !drop.contains(&key.as_str())
        })
        .collect();
    format!("{tag}:{}", kept.join(","))
}

/// 主清单里选中的变体，以及它要配的独立音轨（音频混在视频分片里时为 None）。
#[derive(Debug, PartialEq)]
pub(crate) struct Variant {
    pub uri: String,
    pub stream_inf: String,
    pub audio: Option<Audio>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Audio {
    pub uri: String,
    pub media: String,
}

/// 选带宽最高的变体；它声明了 AUDIO 组时，选组里 DEFAULT → AUTOSELECT → 第一条音轨。
/// 不是主清单返回 None。
pub(crate) fn pick_variant(base: &str, text: &str) -> Option<Variant> {
    let mut best: Option<(u64, String, String)> = None;
    let mut pending: Option<(u64, String)> = None;
    let mut audio_media: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with("#EXT-X-STREAM-INF:") {
            let bw = attr(line, "BANDWIDTH").and_then(|v| v.parse().ok()).unwrap_or(0);
            pending = Some((bw, line.to_string()));
        } else if line.starts_with("#EXT-X-MEDIA:") {
            if attr(line, "TYPE").is_some_and(|t| t.eq_ignore_ascii_case("AUDIO")) {
                audio_media.push(line.to_string());
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some((bw, inf)) = pending.take() {
                if best.as_ref().is_none_or(|(b, _, _)| bw > *b) {
                    best = Some((bw, inf, absolutize(base, line)));
                }
            }
        }
    }
    let (_, stream_inf, uri) = best?;
    let audio = attr(&stream_inf, "AUDIO").and_then(|group| {
        let group_lines: Vec<&String> =
            audio_media.iter().filter(|l| attr(l, "GROUP-ID").as_deref() == Some(group.as_str())).collect();
        let yes = |l: &&&String, k: &str| attr(l, k).is_some_and(|v| v.eq_ignore_ascii_case("YES"));
        let chosen = group_lines
            .iter()
            .find(|l| yes(l, "DEFAULT"))
            .or_else(|| group_lines.iter().find(|l| yes(l, "AUTOSELECT")))
            .or_else(|| group_lines.first())?;
        // 选中的音轨没有 URI = 音频就混在视频分片里，不需要单独下载。
        let audio_uri = attr(chosen, "URI")?;
        Some(Audio { uri: absolutize(base, &audio_uri), media: chosen.to_string() })
    });
    Some(Variant { uri, stream_inf, audio })
}

/// 本地主清单：只留选中的变体和音轨，去掉指向已丢弃组的引用。
pub(crate) fn build_master(variant: &Variant) -> String {
    let mut out = String::from("#EXTM3U\n");
    let mut drop = vec!["SUBTITLES", "VIDEO", "CLOSED-CAPTIONS"];
    match &variant.audio {
        Some(audio) => {
            let media = without_attrs(&audio.media, &["DEFAULT", "AUTOSELECT"]);
            let media = map_uri_attributes(&media, |_| "audio.m3u8".to_string());
            out.push_str(&format!("{media},DEFAULT=YES,AUTOSELECT=YES\n"));
        }
        None => drop.push("AUDIO"),
    }
    out.push_str(&without_attrs(&variant.stream_inf, &drop));
    out.push_str("\nvideo.m3u8\n");
    out
}

/// 把一份媒体播放列表改写成相对文件名。`seg` 是分片文件名前缀（视频 s / 音频 a）。
fn localize_media(namer: &mut Namer, base: &str, text: &str, seg: char) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut ended = false;
    let mut segments = 0;
    for raw in text.trim_start_matches('\u{feff}').lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            if line.starts_with("#EXT-X-STREAM-INF") {
                return Err("播放列表嵌套了主清单".into());
            }
            if line == "#EXT-X-ENDLIST" {
                ended = true;
            }
            if line.starts_with("#EXT-X-KEY:") {
                out.push_str(&map_uri_attributes(line, |uri| namer.intern(absolutize(base, uri), 'k', "key")));
            } else if line.starts_with("#EXT-X-MAP:") {
                out.push_str(&map_uri_attributes(line, |uri| namer.intern(absolutize(base, uri), 'm', "mp4")));
            } else if line.contains("URI=\"") {
                // 其他外部资源（内容引导、插播、低延迟 PART 等）离线无意义，整行丢弃。
                continue;
            } else {
                out.push_str(line);
            }
        } else {
            segments += 1;
            out.push_str(&namer.intern(absolutize(base, line), seg, "ts"));
        }
        out.push('\n');
    }
    if !ended {
        return Err("直播流不支持离线下载".into());
    }
    if segments == 0 {
        return Err("播放列表里没有分片".into());
    }
    Ok(out)
}

/// 单个媒体播放列表（音频混在分片里）：直接作为 index.m3u8。
pub(crate) fn localize(base: &str, text: &str) -> Result<Plan, String> {
    let mut namer = Namer::default();
    let text = localize_media(&mut namer, base, text, 's')?;
    Ok(Plan { files: namer.files, outputs: vec![("index.m3u8".into(), text)] })
}

/// 主清单：本地写 video.m3u8 (+ audio.m3u8) + index.m3u8（主清单）。
pub(crate) fn localize_master(variant: &Variant, video: &str, audio: Option<&str>) -> Result<Plan, String> {
    let mut namer = Namer::default();
    let mut outputs = vec![("video.m3u8".to_string(), localize_media(&mut namer, &variant.uri, video, 's')?)];
    match (&variant.audio, audio) {
        (Some(track), Some(text)) => outputs.push(("audio.m3u8".into(), localize_media(&mut namer, &track.uri, text, 'a')?)),
        (None, None) => {}
        _ => return Err("音轨与清单不匹配".into()),
    }
    outputs.push(("index.m3u8".into(), build_master(variant)));
    Ok(Plan { files: namer.files, outputs })
}

async fn fetch_text(url: &str) -> Result<String, String> {
    let (_, bytes) = fetch_bytes(url).await?;
    let text = String::from_utf8(bytes).map_err(|_| "播放列表不是 UTF-8 文本".to_string())?;
    if !text.trim_start_matches('\u{feff}').starts_with("#EXTM3U") {
        return Err("响应不是 HLS 播放列表".into());
    }
    Ok(text)
}

async fn fetch_plan(url: &str) -> Result<Plan, String> {
    let text = fetch_text(url).await?;
    let Some(variant) = pick_variant(url, &text) else {
        return localize(url, &text);
    };
    let video = fetch_text(&variant.uri).await?;
    let audio = match &variant.audio {
        Some(track) => Some(fetch_text(&track.uri).await?),
        None => None,
    };
    localize_master(&variant, &video, audio.as_deref())
}

/* ───────────────────────── 下载任务 ───────────────────────── */

enum FetchErr {
    Cancelled,
    Expired(String),
    Failed(String),
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // 临时名带两段扩展名，不会被 valid_file 当作可回放文件。
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

async fn fetch_all(dir: &Path, key: &str, files: Vec<(String, String)>, cancel: &Arc<AtomicBool>) -> Result<(), FetchErr> {
    let files = Arc::new(files);
    let next = Arc::new(AtomicUsize::new(0));
    let stop: Arc<Mutex<Option<FetchErr>>> = Arc::new(Mutex::new(None));
    let mut workers = Vec::new();
    for _ in 0..CONCURRENCY {
        let (files, next, stop, cancel) = (files.clone(), next.clone(), stop.clone(), cancel.clone());
        let (dir, key) = (dir.to_path_buf(), key.to_string());
        workers.push(tokio::spawn(async move {
            loop {
                if cancel.load(Ordering::Acquire) || stop.lock().unwrap().is_some() {
                    return;
                }
                let i = next.fetch_add(1, Ordering::AcqRel);
                let Some((remote, name)) = files.get(i) else { return };
                let path = dir.join(name);
                if path.is_file() {
                    continue;
                }
                match fetch_bytes(remote).await {
                    Ok((_, bytes)) => {
                        if cancel.load(Ordering::Acquire) {
                            return;
                        }
                        if let Err(e) = write_atomic(&path, &bytes) {
                            *stop.lock().unwrap() = Some(FetchErr::Failed(format!("写入失败：{e}")));
                            return;
                        }
                        let len = bytes.len() as u64;
                        let mut persist = false;
                        update(&key, false, |item| {
                            item.done += 1;
                            item.bytes += len;
                            persist = item.done % 25 == 0;
                        });
                        if persist {
                            save();
                        }
                    }
                    Err(e) => {
                        let mut slot = stop.lock().unwrap();
                        if slot.is_none() {
                            // 签名过期一般是 401/403/404/410，统一重新取地址续传。
                            *slot = Some(if e.starts_with("HTTP 4") { FetchErr::Expired(e) } else { FetchErr::Failed(e) });
                        }
                        return;
                    }
                }
            }
        }));
    }
    for w in workers {
        let _ = w.await;
    }
    if cancel.load(Ordering::Acquire) {
        return Err(FetchErr::Cancelled);
    }
    let err = stop.lock().unwrap().take();
    err.map_or(Ok(()), Err)
}

/// 续传按序号复用已下文件；换线路或隔几天后清单结构可能变了（分片数、时长、byterange），
/// 这时旧分片不能混进来。本地化后的清单不含 URL，直接逐字比较。
fn reconcile(dir: &Path, text: &str) -> Result<(), String> {
    let sidecar = dir.join("plan.m3u8");
    if std::fs::read_to_string(&sidecar).ok().as_deref() == Some(text) {
        return Ok(());
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name != "cover" && (valid_file(&name) || name.ends_with(".part")) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    write_atomic(&sidecar, text.as_bytes()).map_err(|e| format!("写入下载清单失败：{e}"))
}

fn count_existing(dir: &Path, files: &[(String, String)]) -> (usize, u64) {
    files.iter().fold((0, 0), |(n, bytes), (_, name)| match std::fs::metadata(dir.join(name)) {
        Ok(m) if m.is_file() => (n + 1, bytes + m.len()),
        _ => (n, bytes),
    })
}

/// `resolve` 每次调用都返回一份新的播放地址（签名过期后重新获取）。
async fn run<F, Fut>(key: &str, cover_url: String, cancel: Arc<AtomicBool>, resolve: F) -> Result<(), String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let dir = root()?.join(key);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败：{e}"))?;
    if !cover_url.is_empty() && !dir.join("cover").is_file() {
        if let Ok((_, bytes)) = crate::media::load_image(&cover_url).await {
            if write_atomic(&dir.join("cover"), &bytes).is_ok() {
                update(key, true, |item| item.has_cover = true);
            }
        }
    }
    let mut refreshes = 0;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(CANCELLED.into());
        }
        let info = resolve().await?;
        let url = info
            .get("video_url")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or("没有拿到播放地址")?
            .to_string();
        let plan = fetch_plan(&url).await?;
        reconcile(&dir, &plan.signature())?;
        let (done, bytes) = count_existing(&dir, &plan.files);
        update(key, true, |item| {
            item.total = plan.files.len();
            item.done = done;
            item.bytes = bytes;
        });
        match fetch_all(&dir, key, plan.files, &cancel).await {
            Ok(()) => {
                // index.m3u8 排在最后：它出现即代表视频、音轨、key 全部就绪。
                for (name, text) in &plan.outputs {
                    write_atomic(&dir.join(name), text.as_bytes()).map_err(|e| format!("写入播放列表失败：{e}"))?;
                }
                return Ok(());
            }
            Err(FetchErr::Cancelled) => return Err(CANCELLED.into()),
            Err(FetchErr::Expired(e)) if refreshes >= MAX_REFRESH => return Err(e),
            Err(FetchErr::Expired(_)) => refreshes += 1,
            Err(FetchErr::Failed(e)) => return Err(e),
        }
    }
}

/// 新建或继续下载。已在下载中则直接返回当前条目。
pub fn start<F, Fut>(id: i64, ep: u32, title: String, cover_url: String, resolve: F) -> Result<Item, String>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Value, String>> + Send,
{
    root()?;
    let key = task_key(id, ep);
    let cancel = {
        let mut tasks = TASKS.lock().unwrap();
        if let Some(running) = tasks.get(&key) {
            if running.load(Ordering::Acquire) {
                return Err("上一个任务正在停止，请稍后再试".into());
            }
            return ITEMS.lock().unwrap().iter().find(|i| i.key == key).cloned().ok_or("下载任务不存在".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        tasks.insert(key.clone(), cancel.clone());
        cancel
    };
    let item = {
        let mut items = ITEMS.lock().unwrap();
        if !items.iter().any(|i| i.key == key) {
            items.push(Item {
                key: key.clone(), video_id: id, ep, title: title.clone(), cover_url: cover_url.clone(),
                has_cover: false, status: Status::Paused, done: 0, total: 0, bytes: 0, error: None, created_at: now(),
            });
        }
        let item = items.iter_mut().find(|i| i.key == key).unwrap();
        if item.status != Status::Done {
            item.status = Status::Downloading;
            item.error = None;
            if !title.is_empty() {
                item.title = title;
            }
            if !cover_url.is_empty() {
                item.cover_url = cover_url;
            }
        }
        item.clone()
    };
    if item.status == Status::Done {
        TASKS.lock().unwrap().remove(&key);
        return Ok(item);
    }
    save();
    let cover = item.cover_url.clone();
    tauri::async_runtime::spawn(async move {
        let result = run(&key, cover, cancel, resolve).await;
        TASKS.lock().unwrap().remove(&key);
        update(&key, true, |item| match result {
            Ok(()) => {
                item.status = Status::Done;
                item.error = None;
            }
            Err(e) if e == CANCELLED => item.status = Status::Paused,
            Err(e) => {
                item.status = Status::Failed;
                item.error = Some(e);
            }
        });
    });
    Ok(item)
}

pub fn pause(key: &str) {
    if let Some(cancel) = TASKS.lock().unwrap().get(key) {
        cancel.store(true, Ordering::Release);
    }
}

pub fn delete(key: &str) -> Result<(), String> {
    if !valid_key(key) {
        return Err("无效的下载项".into());
    }
    pause(key);
    ITEMS.lock().unwrap().retain(|i| i.key != key);
    save();
    let dir = root()?.join(key);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("删除文件失败：{e}"))?;
    }
    Ok(())
}

/* ───────────────────────── 本地回放 ───────────────────────── */

/// `/dl/<key>/<file>`：校验路径后读取本地文件，返回 (内容类型, 字节)。
pub fn read(rest: &str) -> Option<(String, Vec<u8>)> {
    let (key, file) = rest.split_once('/')?;
    if !valid_key(key) || !valid_file(file) {
        return None;
    }
    let root = root().ok()?;
    let path = root.join(key).join(file);
    if !path.starts_with(root) {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let ctype = match file.rsplit_once('.').map(|(_, e)| e) {
        _ if file == "cover" => crate::media::image_kind(&bytes).unwrap_or("image/jpeg"),
        Some("m3u8") => "application/vnd.apple.mpegurl",
        Some("ts") => "video/mp2t",
        Some("mp4" | "m4s" | "m4v") => "video/mp4",
        Some("aac") => "audio/aac",
        _ => "application/octet-stream",
    };
    Some((ctype.to_string(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localize_rewrites_keys_maps_segments_and_dedupes_byteranges() {
        let base = "https://cdn.example/v/abc/index.m3u8?auth_key=1";
        let text = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin?auth_key=1\",IV=0x01\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\n#EXT-X-BYTERANGE:100@0\nmain.m4s?auth_key=1\n#EXTINF:4,\n#EXT-X-BYTERANGE:100@100\nmain.m4s?auth_key=1\n#EXT-X-CONTENT-STEERING:SERVER-URI=\"https://evil.example/s.json\"\n#EXT-X-ENDLIST\n";
        let plan = localize(base, text).unwrap();
        let local = &plan.outputs[0].1;
        assert_eq!(plan.outputs[0].0, "index.m3u8");
        assert_eq!(plan.files.len(), 3);
        assert_eq!(plan.files[0], ("https://cdn.example/v/abc/key.bin?auth_key=1".into(), "k00000.bin".into()));
        assert_eq!(plan.files[1].1, "m00001.mp4");
        assert_eq!(plan.files[2].1, "s00002.m4s");
        assert!(local.contains("URI=\"k00000.bin\",IV=0x01"));
        assert!(local.contains("#EXT-X-MAP:URI=\"m00001.mp4\""));
        assert_eq!(local.matches("s00002.m4s").count(), 2);
        assert!(local.contains("#EXT-X-BYTERANGE:100@100"));
        assert!(!local.contains("http") && !local.contains("STEERING"));
        assert!(plan.files.iter().all(|(_, n)| valid_file(n)));
    }

    #[test]
    fn localize_defaults_extensions_and_rejects_live_streams() {
        let plan = localize("https://cdn.example/a/b.m3u8", "#EXTM3U\n#EXTINF:4,\nseg?id=1\n#EXT-X-ENDLIST\n").unwrap();
        assert_eq!(plan.files[0], ("https://cdn.example/a/seg?id=1".into(), "s00000.ts".into()));
        assert!(localize("https://cdn.example/a/b.m3u8", "#EXTM3U\n#EXTINF:4,\nseg.ts\n").is_err());
    }

    #[test]
    fn master_playlist_picks_highest_bandwidth() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2400000,CODECS=\"avc1.64001f,mp4a.40.2\"\nhigh/index.m3u8\n";
        let variant = pick_variant("https://cdn.example/m.m3u8", text).unwrap();
        assert_eq!(variant.uri, "https://cdn.example/high/index.m3u8");
        assert_eq!(variant.audio, None, "audio muxed in variant");
        assert_eq!(build_master(&variant), "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2400000,CODECS=\"avc1.64001f,mp4a.40.2\"\nvideo.m3u8\n");
        assert_eq!(pick_variant("https://cdn.example/m.m3u8", "#EXTM3U\n#EXTINF:4,\na.ts\n"), None);
    }

    #[test]
    fn separate_audio_track_is_downloaded_with_video() {
        let master = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=NO,AUTOSELECT=YES,URI=\"audio/en.m3u8\"\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"中文\",LANGUAGE=\"zh\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio/zh.m3u8\"\n\
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"字幕\",URI=\"subs.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=900000,AUDIO=\"aac\",SUBTITLES=\"subs\"\nlow.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"aac\",SUBTITLES=\"subs\"\nhigh.m3u8\n";
        let variant = pick_variant("https://cdn.example/v/master.m3u8", master).unwrap();
        assert_eq!(variant.uri, "https://cdn.example/v/high.m3u8");
        let audio = variant.audio.as_ref().expect("default audio track");
        assert_eq!(audio.uri, "https://cdn.example/v/audio/zh.m3u8");

        let video = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"/key.bin\"\n#EXTINF:4,\nv1.ts\n#EXTINF:4,\nv2.ts\n#EXT-X-ENDLIST\n";
        let sound = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"/key.bin\"\n#EXTINF:4,\na1.aac\n#EXTINF:4,\na2.aac\n#EXT-X-ENDLIST\n";
        let plan = localize_master(&variant, video, Some(sound)).unwrap();
        let names: Vec<&str> = plan.files.iter().map(|(_, n)| n.as_str()).collect();
        // 共用的 key 只下一次；音频分片单独编号并全部纳入下载。
        assert_eq!(names, ["k00000.bin", "s00001.ts", "s00002.ts", "a00003.aac", "a00004.aac"]);
        assert!(names.iter().all(|n| valid_file(n)));
        let outputs: Vec<&str> = plan.outputs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(outputs, ["video.m3u8", "audio.m3u8", "index.m3u8"]);
        assert!(plan.outputs[1].1.contains("a00003.aac") && plan.outputs[1].1.contains("k00000.bin"));
        let index = &plan.outputs[2].1;
        assert!(index.contains("TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"中文\",LANGUAGE=\"zh\",URI=\"audio.m3u8\",DEFAULT=YES,AUTOSELECT=YES"), "{index}");
        assert!(index.contains("AUDIO=\"aac\"") && index.ends_with("\nvideo.m3u8\n"));
        assert!(!index.contains("SUBTITLES") && !index.contains("English") && !index.contains("http"));
        // 有音轨却没取到音轨清单，必须报错而不是悄悄下出无声视频。
        assert!(localize_master(&variant, video, None).is_err());
    }

    #[test]
    fn audio_group_without_uri_means_muxed_audio() {
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"main\",DEFAULT=YES\n#EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\nv.m3u8\n";
        let variant = pick_variant("https://cdn.example/m.m3u8", master).unwrap();
        assert_eq!(variant.audio, None);
        assert!(!build_master(&variant).contains("AUDIO="), "dangling group reference removed");
    }

    #[test]
    fn changed_plan_discards_stale_segments_but_keeps_cover() {
        let dir = std::env::temp_dir().join(format!("hghubx-dl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        reconcile(&dir, "plan-a").unwrap();
        for name in ["s00001.ts", "k00000.key", "cover", "s00002.ts.part"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        reconcile(&dir, "plan-a").unwrap();
        assert!(dir.join("s00001.ts").is_file(), "same plan keeps segments");
        reconcile(&dir, "plan-b").unwrap();
        assert!(!dir.join("s00001.ts").exists() && !dir.join("k00000.key").exists() && !dir.join("s00002.ts.part").exists());
        assert!(dir.join("cover").is_file());
        assert_eq!(std::fs::read_to_string(dir.join("plan.m3u8")).unwrap(), "plan-b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 联网端到端：下载一集并经 `/dl/` 读回。`cargo test --lib live_download -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "联网测试：依赖上游实时播放地址"]
    async fn live_download_and_serve() {
        use crate::media::{handle, LOCAL_ORIGIN};
        let app_data = std::env::temp_dir().join(format!("hghubx-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&app_data);
        bind(&app_data);
        let cancel = Arc::new(AtomicBool::new(false));
        let api: &'static crate::api::Api = Box::leak(Box::new(crate::api::Api::new()));
        run("117_3", String::new(), cancel, move || crate::api::play_info(api, 117, 3))
        .await
        .expect("download");
        let playlist = handle("/dl/117_3/index.m3u8", "", LOCAL_ORIGIN, None).await;
        assert_eq!(playlist.status(), 200);
        let text = String::from_utf8_lossy(playlist.body()).to_string();
        println!("{}", &text[..text.len().min(300)]);
        // 主清单时分片在 video.m3u8 / audio.m3u8 里
        let media = if text.contains("#EXT-X-STREAM-INF") {
            String::from_utf8_lossy(handle("/dl/117_3/video.m3u8", "", LOCAL_ORIGIN, None).await.body()).to_string()
        } else {
            text.clone()
        };
        let seg_name = media.lines().find(|l| l.starts_with('s')).expect("segment line");
        let seg = handle(&format!("/dl/117_3/{seg_name}"), "", LOCAL_ORIGIN, None).await;
        assert_eq!(seg.status(), 200);
        assert!(seg.body().len() > 1000);
        let key_line = media.lines().find(|l| l.starts_with("#EXT-X-KEY:") && l.contains("AES-128"));
        let mut plain = seg.body().clone();
        if let Some(key_line) = key_line {
            let key_name = attr(key_line, "URI").unwrap();
            let key = handle(&format!("/dl/117_3/{key_name}"), "", LOCAL_ORIGIN, None).await;
            assert_eq!(key.status(), 200);
            assert_eq!(key.body().len(), 16);
            // 第一个分片：IV 取标签里的值，否则按 HLS 规范用媒体序号
            let iv = match attr(key_line, "IV") {
                Some(hex) => {
                    let hex = hex.trim_start_matches("0x").trim_start_matches("0X");
                    (0..16).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap()).collect::<Vec<u8>>()
                }
                None => {
                    let seq: u128 = media.lines().find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                    seq.to_be_bytes().to_vec()
                }
            };
            use aes::cipher::{BlockDecrypt, KeyInit};
            let cipher = aes::Aes128::new_from_slice(key.body()).unwrap();
            let mut prev: [u8; 16] = iv.try_into().unwrap();
            plain = Vec::with_capacity(seg.body().len());
            for chunk in seg.body().chunks(16) {
                let mut block = [0u8; 16];
                block.copy_from_slice(chunk);
                let ct = block;
                cipher.decrypt_block((&mut block).into());
                plain.extend(block.iter().zip(prev).map(|(b, p)| b ^ p));
                prev = ct;
            }
        }
        assert_eq!(plain[0], 0x47, "decrypted segment is MPEG-TS");
        // MPEG-TS 里音频 PES 的 stream_id 为 0xC0-0xDF（AAC/MP3）或 0xBD（AC-3 等私有流）
        let has_audio = plain.windows(4).any(|w| w[..3] == [0, 0, 1] && ((0xC0..=0xDF).contains(&w[3]) || w[3] == 0xBD));
        let has_video = plain.windows(4).any(|w| w[..3] == [0, 0, 1] && (0xE0..=0xEF).contains(&w[3]));
        println!("segment {seg_name}: video={has_video} audio={has_audio}");
        assert!(has_video && has_audio, "下载的分片必须同时含视频与音频");
        let _ = std::fs::remove_dir_all(&app_data);
    }

    #[test]
    fn served_paths_reject_traversal_and_unknown_files() {
        for key in ["123_4", "1_1"] {
            assert!(valid_key(key), "{key}");
        }
        for key in ["..", "123", "_1", "1_", "1_2_3", "a_1", "1/_2", "1_2/..", ""] {
            assert!(!valid_key(key), "{key}");
        }
        for file in ["index.m3u8", "cover", "s00012.ts", "k00000.key", "m00001.mp4"] {
            assert!(valid_file(file), "{file}");
        }
        for file in ["../x", "..", "s0001.ts", "s00012.TS", "x00001.ts", "s00001.ts/..", "downloads.json", "s00001.ts.part", "s00001.ts?x"] {
            assert!(!valid_file(file), "{file}");
        }
        assert!(read("../../etc/passwd").is_none());
        assert!(read("1_1/../../downloads.json").is_none());
    }
}
