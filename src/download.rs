//! 多线路测速 + 多线程分段下载（断点续传 / 暂停 / MD5 / 线路健康度）。

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::logger;
use crate::net;

const SAMPLE: u64 = 256 * 1024;
const MIN_BLOCK: u64 = 1 << 20;
const MAX_BLOCK: u64 = 8 << 20;
const BLOCKS_PER_CONN: u64 = 40;

#[derive(Serialize, Deserialize)]
struct Meta {
    url: String,
    size: u64,
    block: u64,
    done: Vec<u64>,
}

pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub cancel: AtomicBool,
    pub paused: AtomicBool,
    pub host: Mutex<String>,
    pub phase: Mutex<String>,
    pub error: Mutex<String>,
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
            error: Mutex::new(String::new()),
        })
    }
    pub fn set_phase(&self, text: &str) {
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

fn meta_path(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.meta", dest.display()))
}

fn part_path(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part", dest.display()))
}

/// 原子写：先写 .tmp 再 rename，断电不会留下半个 JSON
fn save_meta(path: &Path, meta: &Meta) {
    let Ok(text) = serde_json::to_string(meta) else { return };
    let tmp = path.with_extension("meta.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn probe(url: &str) -> Option<(f64, String, u64)> {
    let started = Instant::now();
    let mut resp = net::probe_client()
        .get(url)
        .header("Referer", net::REFERER)
        .header("Range", format!("bytes=0-{}", SAMPLE - 1))
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let status = resp.status().as_u16();
    let size = if status == 206 {
        resp.headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.trim().parse::<u64>().ok())
    } else {
        resp.content_length()
    };
    let mut got = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    while got < SAMPLE {
        match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got += n as u64,
            Err(_) => break,
        }
    }
    let secs = started.elapsed().as_secs_f64().max(0.05);
    Some((got as f64 / secs, url.to_string(), size?))
}

/// 测速并返回最快的几条线路（按速度降序）
///
/// 最多等 4 秒：某些线路 DNS/连接很慢时，不能让整个下载卡在测速上。
fn pick_sources(candidates: &[String], keep: usize) -> Vec<(String, u64)> {
    use std::sync::mpsc;

    let (sender, receiver) = mpsc::channel::<(f64, String, u64)>();
    for url in candidates.iter().take(8) {
        let url = url.clone();
        let sender = sender.clone();
        thread::spawn(move || {
            if let Some(info) = probe(&url) {
                let _ = sender.send(info);
            }
        });
    }
    drop(sender);

    let deadline = Instant::now() + Duration::from_millis(4000);
    let mut results: Vec<(f64, String, u64)> = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(remaining) {
            Ok(info) => results.push(info),
            Err(_) => break,
        }
    }

    results.retain(|(_, _, size)| *size > 0);
    results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    for (speed, url, _) in &results {
        logger::log(&format!("probe {:.0} KB/s  {url}", speed / 1024.0));
    }
    results
        .into_iter()
        .take(keep)
        .map(|(_, url, size)| (url, size))
        .collect()
}

/// 测速没结果时的兜底：按优先级逐个探测，拿到文件大小就返回
fn fallback_source(pool: &[String]) -> Option<(String, u64)> {
    let mut ordered: Vec<&String> = pool.iter().collect();
    ordered.sort_by_key(|url| {
        if url.contains("cdnorg") {
            0
        } else if url.contains("bn.d.miui") {
            1
        } else if url.contains("bksgp") || url.contains("aliyuncs") {
            2
        } else if url.contains("bigota") {
            3
        } else {
            4
        }
    });
    for url in ordered {
        if let Some((_, found, size)) = probe(url) {
            return Some((found, size));
        }
    }
    None
}

fn fetch_block(
    sources: &[String],
    health: &Mutex<HashMap<String, u32>>,
    file: &std::fs::File,
    start: u64,
    end: u64,
    progress: &Progress,
    start_source: usize,
) -> Result<(), String> {
    let mut last = String::from("下载失败");
    let mut attempt = 0usize;
    while attempt < 8 {
        if progress.cancel.load(Ordering::Relaxed) || progress.paused.load(Ordering::Relaxed) {
            return Ok(());
        }
        let url = {
            let guard = health.lock().unwrap();
            // 优先用没怎么失败过的线路
            let mut ordered: Vec<&String> = sources.iter().collect();
            ordered.sort_by_key(|u| guard.get(*u).copied().unwrap_or(0));
            ordered[(start_source + attempt) % ordered.len()].clone()
        };
        match net::client()
            .get(url.as_str())
            .header("Referer", net::REFERER)
            .header("Range", format!("bytes={start}-{end}"))
            .send()
        {
            Ok(mut resp) if resp.status().is_success() => {
                let mut offset = start;
                let mut buf = vec![0u8; 256 * 1024];
                let mut failed = false;
                loop {
                    if progress.cancel.load(Ordering::Relaxed) || progress.paused.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    match resp.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            file.seek_write(&buf[..n], offset).map_err(|e| e.to_string())?;
                            offset += n as u64;
                            progress.done.fetch_add(n as u64, Ordering::Relaxed);
                        }
                        Err(err) => {
                            last = err.to_string();
                            failed = true;
                            break;
                        }
                    }
                }
                if !failed && offset == end + 1 {
                    health.lock().unwrap().remove(&url);
                    return Ok(());
                }
                if !failed {
                    last = "连接中断".into();
                }
                health.lock().unwrap().entry(url.clone()).or_insert(0);
                *health.lock().unwrap().get_mut(&url).unwrap() += 1;
            }
            Ok(resp) => {
                last = format!("HTTP {}", resp.status());
                *health.lock().unwrap().entry(url.clone()).or_insert(0) += 1;
            }
            Err(err) => {
                last = err.to_string();
                *health.lock().unwrap().entry(url.clone()).or_insert(0) += 1;
            }
        }
        attempt += 1;
        let backoff = (300u64 * (1 << attempt.min(3))).min(2000);
        logger::log(&format!(
            "block retry {}/{} after {} ms: {}",
            attempt, 8, backoff, last
        ));
        thread::sleep(Duration::from_millis(backoff));
    }
    Err(last)
}

/// 剩余空间检查（字节）
fn free_space(folder: &Path) -> Option<u64> {
    #[repr(C)]
    struct U64Parts {
        low: u32,
        high: u32,
    }
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }
    let mut wide: Vec<u16> = folder.to_string_lossy().encode_utf16().collect();
    wide.push(0);
    let mut free = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free)
    };
    if ok != 0 { Some(free) } else { None }
}

fn verify_md5(path: &Path, expected: &str) -> Result<bool, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut context = md5::Context::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        context.consume(&buf[..n]);
    }
    Ok(format!("{:x}", context.compute()).eq_ignore_ascii_case(expected))
}

pub fn run(
    candidates: Vec<String>,
    dest: PathBuf,
    conns: usize,
    expected_md5: Option<String>,
    progress: Arc<Progress>,
) -> Result<Outcome, String> {
    if dest.exists() {
        if let Some(expected) = &expected_md5 {
            if verify_md5(&dest, expected)? {
                let size = dest.metadata().map(|m| m.len()).unwrap_or(0);
                progress.total.store(size, Ordering::Relaxed);
                progress.done.store(size, Ordering::Relaxed);
                progress.set_phase("已完成");
                return Ok(Outcome::Done);
            }
        }
    }

    progress.set_phase("测速选线");
    let pool: Vec<String> = if candidates.len() > 1 {
        candidates.clone()
    } else {
        net::mirror_candidates(candidates.first().map(String::as_str).unwrap_or(""))
    };
    let mut top = pick_sources(&pool, 4);
    if top.is_empty() {
        logger::log("probe timeout, fallback to sequential check");
        if let Some(found) = fallback_source(&pool) {
            top.push(found);
        }
    }
    let Some((best_url, total)) = top.first().cloned() else {
        return Err("所有下载线路都连接失败".into());
    };
    let mut sources: Vec<String> = top.iter().map(|(url, _)| url.clone()).collect();
    for url in &pool {
        if sources.len() >= 4 {
            break;
        }
        if !sources.contains(url) {
            sources.push(url.clone());
        }
    }
    logger::log(&format!(
        "download start: {best_url} size={total} sources={}",
        sources.len()
    ));

    progress.total.store(total, Ordering::Relaxed);
    *progress.host.lock().unwrap() = best_url
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let part = part_path(&dest);
        let need = if part.exists() {
            total.saturating_sub(part.metadata().map(|m| m.len()).unwrap_or(0))
        } else {
            total
        };
        if let Some(free) = free_space(parent) {
            if free < need + 64 * 1024 * 1024 {
                return Err(format!(
                    "磁盘空间不足：还需要 {:.1} GB，剩余 {:.1} GB",
                    need as f64 / 1073741824.0,
                    free as f64 / 1073741824.0
                ));
            }
        }
    }

    // 块更小 → 慢连接只占小块，天然更均衡
    let target = (conns as u64 * BLOCKS_PER_CONN).clamp(1, 4000);
    let block = (total / target).clamp(MIN_BLOCK, MAX_BLOCK);
    let blocks = total.div_ceil(block);
    logger::log(&format!("block={} bytes, blocks={}", block, blocks));

    let part = part_path(&dest);
    let meta_file = meta_path(&dest);

    let mut done_set: HashSet<u64> = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(&meta_file) {
        if let Ok(meta) = serde_json::from_str::<Meta>(&text) {
            if meta.size == total && meta.block == block && part.exists() {
                done_set = meta.done.into_iter().filter(|i| *i < blocks).collect();
                logger::log(&format!("resume: {}/{} blocks done", done_set.len(), blocks));
            }
        }
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .open(&part)
        .map_err(|e| e.to_string())?;
    let existing = file.metadata().map(|m| m.len()).unwrap_or(0);
    if existing != total {
        file.set_len(total).map_err(|e| e.to_string())?;
        done_set.clear();
    }

    progress.done.store(
        done_set
            .iter()
            .map(|i| (i * block + block).min(total) - i * block)
            .sum(),
        Ordering::Relaxed,
    );

    let done = Arc::new(Mutex::new(done_set));
    let cursor = Arc::new(AtomicUsize::new(0));
    let file = Arc::new(file);
    let saver = Arc::new(Mutex::new(Instant::now()));
    let health = Arc::new(Mutex::new(HashMap::<String, u32>::new()));

    progress.set_phase("下载中");
    let workers = conns.max(1);
    let mut handles = Vec::new();
    for _ in 0..workers {
        let cursor = Arc::clone(&cursor);
        let file = Arc::clone(&file);
        let done = Arc::clone(&done);
        let progress = Arc::clone(&progress);
        let saver = Arc::clone(&saver);
        let health = Arc::clone(&health);
        let sources = sources.clone();
        let part = part.clone();
        let meta_file = meta_file.clone();
        let meta_url = best_url.clone();
        handles.push(thread::spawn(move || -> Result<(), String> {
            loop {
                if progress.cancel.load(Ordering::Relaxed) || progress.paused.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let index = cursor.fetch_add(1, Ordering::Relaxed) as u64;
                if index >= blocks {
                    return Ok(());
                }
                if done.lock().unwrap().contains(&index) {
                    continue;
                }
                let start = index * block;
                let end = (start + block - 1).min(total - 1);
                fetch_block(&sources, &health, &file, start, end, &progress, index as usize)?;
                if progress.paused.load(Ordering::Relaxed) || progress.cancel.load(Ordering::Relaxed) {
                    return Ok(());
                }
                done.lock().unwrap().insert(index);
                let mut last = saver.lock().unwrap();
                if last.elapsed() > Duration::from_secs(2) {
                    let meta = Meta {
                        url: meta_url.clone(),
                        size: total,
                        block,
                        done: done.lock().unwrap().iter().copied().collect(),
                    };
                    save_meta(&meta_file, &meta);
                    *last = Instant::now();
                }
            }
        }));
    }

    let mut error = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                if error.is_none() {
                    error = Some(err);
                }
            }
            Err(_) => {
                if error.is_none() {
                    error = Some("下载线程异常退出".into());
                }
            }
        }
    }

    let cancelled = progress.cancel.load(Ordering::Relaxed);
    let paused = progress.paused.load(Ordering::Relaxed);

    if cancelled {
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_file(&meta_file);
        logger::log("download cancelled");
        return Ok(Outcome::Cancelled);
    }

    let finished_blocks = done.lock().unwrap().len() as u64;
    if paused || finished_blocks < blocks {
        let meta = Meta {
            url: best_url.clone(),
            size: total,
            block,
            done: done.lock().unwrap().iter().copied().collect(),
        };
        save_meta(&meta_file, &meta);
        progress.set_phase(if paused { "已暂停" } else { "等待继续" });
        logger::log(&format!(
            "download paused: {}/{} blocks",
            finished_blocks, blocks
        ));
        return Ok(Outcome::Paused);
    }

    if let Some(err) = error {
        logger::log(&format!("download error: {err}"));
        return Err(err);
    }

    if let Some(expected) = &expected_md5 {
        progress.set_phase("校验 MD5");
        if !verify_md5(&part, expected)? {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(&meta_file);
            logger::log("md5 mismatch");
            return Err("MD5 校验失败，文件可能损坏".into());
        }
    }

    std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&meta_file);
    progress.set_phase("已完成");
    logger::log(&format!("download done: {}", dest.display()));
    Ok(Outcome::Done)
}
