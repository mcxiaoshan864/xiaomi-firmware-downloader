//! Multi-CDN downloads, verified byte ranges, and durable resume checkpoints.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::blocking::Response;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_RANGE, ETAG, IF_MATCH, RANGE, REFERER};
use serde::{Deserialize, Serialize};

use crate::{logger, net};

const SAMPLE: u64 = 256 * 1024;
const MIN_BLOCK: u64 = 1 << 20;
const MAX_BLOCK: u64 = 8 << 20;
const RETRIES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Identity {
    Md5(String),
    Etag { url: String, value: String },
}

#[derive(Serialize, Deserialize)]
struct Meta {
    identity: Identity,
    size: u64,
    block: u64,
    done: Vec<u64>,
}

pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub cancel: AtomicBool,
    pub paused: AtomicBool,
    host: Mutex<String>,
    phase: Mutex<String>,
}

impl Progress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            host: Mutex::new(String::new()),
            phase: Mutex::new("连接中".into()),
        })
    }

    fn interruption(&self) -> Option<Outcome> {
        if self.cancel.load(Relaxed) {
            Some(Outcome::Cancelled)
        } else if self.paused.load(Relaxed) {
            Some(Outcome::Paused)
        } else {
            None
        }
    }

    fn set_phase(&self, text: &str) {
        *self.phase.lock().unwrap() = text.to_string();
    }

    pub fn phase(&self) -> String {
        self.phase.lock().unwrap().clone()
    }

    pub fn host(&self) -> String {
        self.host.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Paused,
    Cancelled,
}

pub fn normalized_md5(value: &str) -> Option<String> {
    let value = value.trim();
    (value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn sidecar(dest: &Path, suffix: &str) -> PathBuf {
    let mut path = dest.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

struct Target {
    dest: PathBuf,
    part: PathBuf,
    meta: PathBuf,
    _lock: File,
}

impl Target {
    fn claim(dest: &Path) -> Result<Self, String> {
        let full = std::path::absolute(dest).map_err(|e| e.to_string())?;
        let parent = full.parent().ok_or("下载目录无效")?;
        fs::create_dir_all(parent).map_err(|e| format!("无法创建下载目录：{e}"))?;
        let dest = fs::canonicalize(parent)
            .map_err(|e| format!("无法访问下载目录：{e}"))?
            .join(full.file_name().ok_or("固件文件名无效")?);

        // FILE_FLAG_DELETE_ON_CLOSE lets Windows release the reservation even if we crash.
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .share_mode(0)
            .custom_flags(0x0400_0000)
            .open(sidecar(&dest, ".lock"))
            .map_err(|e| format!("无法独占下载目标，可能有另一个任务正在使用：{e}"))?;
        Ok(Self {
            part: sidecar(&dest, ".part"),
            meta: sidecar(&dest, ".meta"),
            dest,
            _lock: lock,
        })
    }

    fn clear_partial(&self) -> Result<(), String> {
        remove_if_exists(&self.meta).and_then(|()| remove_if_exists(&self.part))
    }
}

pub fn discard_partial(dest: &Path) -> Result<(), String> {
    Target::claim(dest)?.clear_partial()
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("无法移除下载临时文件：{err}")),
    }
}

fn save_meta(path: &Path, file: &File, meta: &Meta) -> Result<(), String> {
    let tmp = path.with_extension("meta.tmp");
    let text = serde_json::to_vec(meta).map_err(|e| e.to_string())?;
    (|| -> io::Result<()> {
        // Commit payload bytes before publishing the blocks that refer to them.
        file.sync_data()?;
        let mut output = File::create(&tmp)?;
        output.write_all(&text)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&tmp, path)
    })()
    .map_err(|e| format!("无法保存断点：{e}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
    total: u64,
}

impl ByteRange {
    fn parse(value: &str) -> Option<Self> {
        let (range, total) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
        let (start, end) = range.split_once('-')?;
        let range = Self {
            start: start.parse().ok()?,
            end: end.parse().ok()?,
            total: total.parse().ok()?,
        };
        (range.start <= range.end && range.end < range.total).then_some(range)
    }

    fn len(self) -> u64 {
        self.end - self.start + 1
    }

    fn check(self, response: &Response) -> Result<(), String> {
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(format!("线路未返回分段响应（HTTP {}）", response.status()));
        }
        let actual = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(Self::parse);
        if actual != Some(self)
            || response.content_length().is_some_and(|size| size != self.len())
        {
            return Err("线路返回的字节范围与请求不一致".into());
        }
        Ok(())
    }
}

fn strong_etag(response: &Response) -> Option<String> {
    let value = response.headers().get(ETAG)?.to_str().ok()?;
    (value.len() >= 2 && value.starts_with('"') && value.ends_with('"')).then(|| value.to_string())
}

#[derive(Debug)]
struct Source {
    url: String,
    total: u64,
    ranged: bool,
    etag: Option<String>,
    speed: f64,
}

fn probe(url: &str) -> Option<Source> {
    let start = Instant::now();
    let mut response = net::probe_client()
        .get(url)
        .header(REFERER, net::REFERER)
        .header(ACCEPT_ENCODING, "identity")
        .header(RANGE, format!("bytes=0-{}", SAMPLE - 1))
        .send()
        .ok()?;
    let ranged = response.status() == StatusCode::PARTIAL_CONTENT;
    let total = if ranged {
        let range = ByteRange::parse(response.headers().get(CONTENT_RANGE)?.to_str().ok()?)?;
        ByteRange { start: 0, end: SAMPLE.min(range.total) - 1, total: range.total }
            .check(&response)
            .ok()?;
        range.total
    } else if response.status() == StatusCode::OK {
        response.content_length()?
    } else {
        return None;
    };
    if total == 0 {
        return None;
    }
    let mut buf = [0; 32 * 1024];
    let sample = SAMPLE.min(total);
    let mut received = 0;
    while received < sample {
        let limit = (sample - received).min(buf.len() as u64) as usize;
        match response.read(&mut buf[..limit]) {
            Ok(0) | Err(_) => break,
            Ok(size) => received += size as u64,
        }
    }
    if received == 0 {
        return None;
    }
    Some(Source {
        url: response.url().to_string(),
        total,
        ranged,
        etag: strong_etag(&response),
        speed: received as f64 / start.elapsed().as_secs_f64().max(0.05),
    })
}

fn select_sources(
    candidates: &[String],
    md5: Option<&str>,
    previous: Option<&Meta>,
    progress: &Progress,
) -> Vec<Source> {
    let mut pool = Vec::new();
    for candidate in candidates {
        for raw in net::mirror_candidates(candidate) {
            if let Some(url) = net::firmware_url(&raw) {
                if (md5.is_some() || url.scheme() == "https") && !pool.contains(&raw) {
                    pool.push(raw);
                }
            }
        }
    }
    let (send, receive) = mpsc::channel();
    for url in pool.iter().take(8).cloned() {
        let send = send.clone();
        thread::spawn(move || {
            if let Some(source) = probe(&url) {
                let _ = send.send(source);
            }
        });
    }
    drop(send);
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut sources = Vec::new();
    while let Some(wait) = deadline.checked_duration_since(Instant::now()) {
        if progress.interruption().is_some() {
            return Vec::new();
        }
        match receive.recv_timeout(wait.min(Duration::from_millis(200))) {
            Ok(source) => sources.push(source),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if sources.is_empty() {
        for url in &pool {
            if progress.interruption().is_some() {
                return Vec::new();
            }
            if let Some(source) = probe(url) {
                sources.push(source);
                break;
            }
        }
    }
    sources.sort_by(|a, b| {
        let ranges = |source: &Source| source.ranged && (md5.is_some() || source.etag.is_some());
        let resumes = |source: &Source| {
            previous.is_some_and(|meta| {
                matches!(&meta.identity, Identity::Etag { url, value }
                    if source.url == *url && source.etag.as_ref() == Some(value))
                    && meta.size == source.total
                    && source.ranged
            })
        };
        resumes(b)
            .cmp(&resumes(a))
            .then_with(|| ranges(b).cmp(&ranges(a)))
            .then_with(|| b.speed.total_cmp(&a.speed))
    });
    if let Some(first) = sources.first() {
        if md5.is_some() && first.ranged {
            let total = first.total;
            let mut seen = HashSet::new();
            sources.retain(|source| {
                source.ranged && source.total == total && seen.insert(source.url.clone())
            });
            sources.truncate(4);
        } else {
            sources.truncate(1);
        }
    }
    sources
}

// A network interruption can be retried. A failed disk write must stop the transfer.
enum CopyError {
    Network(String),
    Disk(String),
}

fn write_at(file: &File, mut input: &[u8], mut offset: u64) -> io::Result<()> {
    while !input.is_empty() {
        let written = file.seek_write(input, offset)?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        offset += written as u64;
        input = &input[written..];
    }
    Ok(())
}

fn copy_body(
    mut body: impl Read,
    file: &File,
    start: u64,
    len: u64,
    stopped: impl Fn() -> bool,
    mut report: impl FnMut(u64),
) -> Result<bool, CopyError> {
    let mut offset = start;
    let end = start + len;
    let mut buf = vec![0; 128 * 1024];
    while offset < end {
        if stopped() {
            return Ok(false);
        }
        let limit = (end - offset).min(buf.len() as u64) as usize;
        let size = body.read(&mut buf[..limit]).map_err(|e| CopyError::Network(e.to_string()))?;
        if size == 0 {
            return Err(CopyError::Network("响应内容提前结束".into()));
        }
        write_at(file, &buf[..size], offset).map_err(|e| CopyError::Disk(e.to_string()))?;
        offset += size as u64;
        report(size as u64);
    }
    if stopped() {
        return Ok(false);
    }
    match body.read(&mut buf[..1]) {
        Ok(0) => Ok(true),
        Ok(_) => Err(CopyError::Network("响应内容超过声明的字节数".into())),
        Err(err) => Err(CopyError::Network(err.to_string())),
    }
}

fn range_response(source: &Source, range: ByteRange, etag: Option<&str>) -> Result<Response, String> {
    let mut request = net::client()
        .get(&source.url)
        .header(REFERER, net::REFERER)
        .header(ACCEPT_ENCODING, "identity")
        .header(RANGE, format!("bytes={}-{}", range.start, range.end));
    if let Some(value) = etag {
        request = request.header(IF_MATCH, value);
    }
    let response = request.send().map_err(|e| e.to_string())?;
    range.check(&response)?;
    if let Some(expected) = etag {
        if response.url().as_str() != source.url || strong_etag(&response).as_deref() != Some(expected) {
            return Err("线路上的固件已变化，无法继续使用旧断点".into());
        }
    }
    Ok(response)
}

struct Transfer<'a> {
    file: &'a File,
    sources: &'a [Source],
    identity: &'a Identity,
    progress: &'a Progress,
    stop: AtomicBool,
}

impl Transfer<'_> {
    fn stopped(&self) -> bool {
        self.stop.load(Relaxed) || self.progress.interruption().is_some()
    }

    fn fetch_block(&self, index: u64, range: ByteRange) -> Result<bool, String> {
        let etag = match self.identity {
            Identity::Etag { value, .. } => Some(value.as_str()),
            Identity::Md5(_) => None,
        };
        let mut last = String::new();
        for attempt in 0..RETRIES {
            if self.stopped() {
                return Ok(false);
            }
            let source = &self.sources[(index as usize + attempt) % self.sources.len()];
            let mut counted = 0;
            let result = range_response(source, range, etag)
                .map_err(CopyError::Network)
                .and_then(|response| {
                    copy_body(
                        response, self.file, range.start, range.len(), || self.stopped(),
                        |size| {
                            counted += size;
                            self.progress.done.fetch_add(size, Relaxed);
                        },
                    )
                });
            // Even bytes written by an incomplete attempt will be overwritten in full.
            // Only publish this block after the complete response has been validated.
            if matches!(result, Ok(true)) {
                return Ok(true);
            }
            self.progress.done.fetch_sub(counted, Relaxed);
            match result {
                Ok(true) => unreachable!(),
                Ok(false) => return Ok(false),
                Err(CopyError::Disk(err)) => return Err(format!("写入固件失败：{err}")),
                Err(CopyError::Network(err)) => last = err,
            }
            if attempt + 1 < RETRIES {
                let wait = 2u64.pow(attempt.min(4) as u32);
                for _ in 0..wait {
                    if self.stopped() {
                        return Ok(false);
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
        Err(format!("分段下载失败（{}–{}）：{last}", range.start, range.end))
    }
}

fn default_block_size(total: u64, workers: usize) -> u64 {
    (total / (workers as u64 * 40)).clamp(MIN_BLOCK, MAX_BLOCK)
}

fn checkpoint_for(
    previous: Option<Meta>,
    identity: &Identity,
    total: u64,
    part_len: u64,
    workers: usize,
) -> (u64, HashSet<u64>) {
    if let Some(meta) = previous {
        if meta.identity == *identity
            && meta.size == total
            && part_len == total
            && (MIN_BLOCK..=MAX_BLOCK).contains(&meta.block)
        {
            let blocks = total.div_ceil(meta.block);
            return (meta.block, meta.done.into_iter().filter(|index| *index < blocks).collect());
        }
    }
    (default_block_size(total, workers), HashSet::new())
}

fn range_at(index: u64, block: u64, total: u64) -> ByteRange {
    let start = index * block;
    ByteRange { start, end: start + block.min(total - start) - 1, total }
}

struct Checkpoint {
    done: HashSet<u64>,
    last_saved: Instant,
}

struct Segments<'a> {
    target: &'a Target,
    file: &'a File,
    sources: &'a [Source],
    progress: &'a Progress,
    identity: Identity,
    block: u64,
    workers: usize,
}

impl Segments<'_> {
    fn run(&self, done: HashSet<u64>) -> Result<(), String> {
        let total = self.sources[0].total;
        let blocks = total.div_ceil(self.block);
        let remaining = blocks - done.len() as u64;
        let cursor = AtomicU64::new(0);
        self.progress.done.store(
            done.iter().map(|&index| range_at(index, self.block, total).len()).sum(),
            Relaxed,
        );
        let state = Mutex::new(Checkpoint { done, last_saved: Instant::now() });
        let transfer = Transfer {
            file: self.file,
            sources: self.sources,
            identity: &self.identity,
            progress: self.progress,
            stop: AtomicBool::new(false),
        };
        let result = thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..self.workers.min(remaining as usize) {
                handles.push(scope.spawn(|| -> Result<(), String> {
                    while !transfer.stopped() {
                        let index = cursor.fetch_add(1, Relaxed);
                        if index >= blocks {
                            break;
                        }
                        if state.lock().unwrap().done.contains(&index) {
                            continue;
                        }
                        let range = range_at(index, self.block, total);
                        match transfer.fetch_block(index, range) {
                            Ok(true) => {}
                            Ok(false) => break,
                            Err(err) => {
                                transfer.stop.store(true, Relaxed);
                                return Err(err);
                            }
                        }
                        let mut checkpoint = state.lock().unwrap();
                        checkpoint.done.insert(index);
                        if checkpoint.last_saved.elapsed() >= Duration::from_secs(2) {
                            if let Err(err) = self.save(total, &checkpoint.done) {
                                logger::log(&err);
                            }
                            checkpoint.last_saved = Instant::now();
                        }
                    }
                    Ok(())
                }));
            }
            let mut result = Ok(());
            for worker in handles {
                let block = worker.join().unwrap_or_else(|_| Err("下载线程异常退出".into()));
                if result.is_ok() {
                    result = block;
                }
            }
            result
        });
        if self.progress.interruption() == Some(Outcome::Cancelled) {
            return Ok(());
        }
        let checkpoint = state.into_inner().unwrap();
        let complete = checkpoint.done.len() as u64 == blocks;
        if !complete || self.progress.interruption().is_some() || result.is_err() {
            if let Err(err) = self.save(total, &checkpoint.done) {
                return Err(match result {
                    Ok(()) => err,
                    Err(original) => format!("{original}。{err}"),
                });
            }
        }
        if result.is_ok() && !complete && self.progress.interruption().is_none() {
            return Err("部分固件分段未完成".into());
        }
        result
    }

    fn save(&self, total: u64, done: &HashSet<u64>) -> Result<(), String> {
        let mut done: Vec<_> = done.iter().copied().collect();
        done.sort_unstable();
        save_meta(
            &self.target.meta, self.file,
            &Meta { identity: self.identity.clone(), size: total, block: self.block, done },
        )
    }
}

fn download_single(source: &Source, file: &File, progress: &Progress) -> Result<(), String> {
    progress.set_phase("单线路下载");
    let response = net::client()
        .get(&source.url)
        .header(REFERER, net::REFERER)
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .map_err(|e| e.to_string())?;
    if response.status() != StatusCode::OK
        || response.content_length().is_some_and(|size| size != source.total)
    {
        return Err(format!("整包响应与测速结果不一致（HTTP {}）", response.status()));
    }
    copy_body(
        response, file, 0, source.total, || progress.interruption().is_some(),
        |size| {
            progress.done.fetch_add(size, Relaxed);
        },
    )
    .map(|_| ())
    .map_err(|err| match err {
        CopyError::Network(message) => format!("整包下载失败：{message}"),
        CopyError::Disk(message) => format!("写入固件失败：{message}"),
    })
}

fn free_space(folder: &Path) -> Option<u64> {
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }
    let wide: Vec<u16> = folder.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut())
    };
    (ok != 0).then_some(free)
}

fn verify_md5(path: &Path, expected: &str, progress: &Progress) -> Result<Option<bool>, String> {
    progress.set_phase("校验 MD5");
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut context = md5::Context::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        if progress.interruption().is_some() {
            return Ok(None);
        }
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        context.consume(&buf[..n]);
    }
    Ok(Some(format!("{:x}", context.compute()) == expected))
}

fn promote_part(from: &Path, to: &Path) -> io::Result<()> {
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // A zero flags value leaves any existing destination untouched.
    let moved = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if moved == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn finish_interruption(target: &Target, outcome: Outcome) -> Result<Outcome, String> {
    if outcome == Outcome::Cancelled {
        target.clear_partial()?;
    }
    Ok(outcome)
}

pub fn run(
    candidates: Vec<String>,
    dest: PathBuf,
    conns: usize,
    expected_md5: Option<String>,
    progress: Arc<Progress>,
) -> Result<Outcome, String> {
    let md5 = expected_md5.as_deref().and_then(normalized_md5);
    let target = Target::claim(&dest)?;
    if let Some(outcome) = progress.interruption() {
        return finish_interruption(&target, outcome);
    }
    if target.dest.exists() {
        if let Some(expected) = md5.as_deref() {
            if verify_md5(&target.dest, expected, &progress)? == Some(true) {
                let size = target.dest.metadata().map_err(|e| e.to_string())?.len();
                progress.total.store(size, Relaxed);
                progress.done.store(size, Relaxed);
                return Ok(Outcome::Done);
            }
        }
        if let Some(outcome) = progress.interruption() {
            return finish_interruption(&target, outcome);
        }
        return Err("目标文件已存在，无法确认与所选固件一致。请检查原文件或更换下载目录".into());
    }

    let previous = fs::read(&target.meta).ok().and_then(|data| serde_json::from_slice(&data).ok());
    progress.done.store(0, Relaxed);
    progress.set_phase("测速选线");
    let sources = select_sources(&candidates, md5.as_deref(), previous.as_ref(), &progress);
    if let Some(outcome) = progress.interruption() {
        return finish_interruption(&target, outcome);
    }
    let first = sources.first().ok_or("没有可用的官方固件下载线路")?;
    let total = first.total;
    let identity = md5.clone().map(Identity::Md5).or_else(|| {
        first.etag.clone().map(|value| Identity::Etag { url: first.url.clone(), value })
    });
    progress.total.store(total, Relaxed);
    *progress.host.lock().unwrap() =
        net::firmware_url(&first.url).and_then(|url| url.host_str().map(str::to_owned)).unwrap_or_default();
    logger::log(&format!("download start: {} bytes, {} sources", total, sources.len()));

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&target.part)
        .map_err(|e| format!("无法打开临时固件文件：{e}"))?;
    let part_len = file.metadata().map_err(|e| e.to_string())?.len();
    let folder = target.dest.parent().ok_or("下载目录无效")?;
    let need = total.saturating_sub(part_len);
    if need > 0 && free_space(folder).is_some_and(|free| free < need) {
        return Err(format!("磁盘空间不足，还需约 {:.2} GiB", need as f64 / (1u64 << 30) as f64));
    }
    let workers = conns.clamp(1, 64);
    let result = if let Some(identity) = identity.filter(|_| first.ranged) {
        let (block, done) = checkpoint_for(previous, &identity, total, part_len, workers);
        if done.is_empty() {
            remove_if_exists(&target.meta)?;
        }
        file.set_len(total).map_err(|e| format!("无法预分配磁盘空间：{e}"))?;
        progress.set_phase("分段下载");
        let segments = Segments { target: &target, file: &file, sources: &sources, progress: &progress, identity, block, workers };
        let result = segments.run(done);
        // Keep the completed checkpoint if the user pauses during final verification.
        if result.is_ok() && progress.interruption().is_none() {
            let complete = (0..total.div_ceil(block)).collect();
            segments.save(total, &complete)?;
        }
        result
    } else {
        remove_if_exists(&target.meta)?;
        file.set_len(total).map_err(|e| format!("无法预分配磁盘空间：{e}"))?;
        download_single(first, &file, &progress)
    };
    if result.is_ok() && progress.interruption().is_none() {
        file.sync_all().map_err(|e| format!("无法将固件写入磁盘：{e}"))?;
    }
    drop(file);
    if progress.interruption() == Some(Outcome::Cancelled) {
        return finish_interruption(&target, Outcome::Cancelled);
    }
    result?;
    if let Some(outcome) = progress.interruption() {
        return finish_interruption(&target, outcome);
    }
    if let Some(expected) = md5.as_deref() {
        if verify_md5(&target.part, expected, &progress)? == Some(false) {
            target.clear_partial()?;
            return Err("MD5 校验失败，已清理未通过校验的临时文件".into());
        }
    }
    if let Some(outcome) = progress.interruption() {
        return finish_interruption(&target, outcome);
    }
    promote_part(&target.part, &target.dest).map_err(|e| format!("无法保存最终固件文件：{e}"))?;
    if let Err(err) = remove_if_exists(&target.meta) {
        logger::log(&err);
    }
    progress.set_phase("已完成");
    logger::log(&format!("download done: {}", target.dest.display()));
    Ok(Outcome::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_ranges_and_checks_the_last_block() {
        let valid = ByteRange { start: 10, end: 19, total: 20 };
        assert_eq!(ByteRange::parse("bytes 10-19/20"), Some(valid));
        for invalid in ["bytes 19-10/20", "bytes 10-20/20", "bytes 10-19/*", "bytes */20"] {
            assert_eq!(ByteRange::parse(invalid), None, "{invalid}");
        }
        assert_eq!(range_at(1, 10, 20), valid);
        assert_eq!(range_at(1, 10, 12), ByteRange { start: 10, end: 11, total: 12 });
    }

    #[test]
    fn checkpoints_require_a_matching_package_and_keep_the_original_block_size() {
        let total = 16 * MAX_BLOCK;
        let old_identity = Identity::Md5("first".into());
        let checkpoint = || Meta {
            identity: old_identity.clone(), size: total, block: MAX_BLOCK, done: vec![0, 15, 16],
        };
        let (block, done) = checkpoint_for(Some(checkpoint()), &old_identity, total, total, 64);
        assert_eq!(block, MAX_BLOCK);
        assert_eq!(done, HashSet::from([0, 15]));
        let different = Identity::Md5("second".into());
        assert!(checkpoint_for(Some(checkpoint()), &different, total, total, 64).1.is_empty());
        assert!(checkpoint_for(Some(checkpoint()), &old_identity, total, 0, 64).1.is_empty());
    }
}
