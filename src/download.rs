//! 多线路测速 + 多线程分段下载（断点续传 / 暂停 / MD5 校验）。

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::net;

const SAMPLE: u64 = 512 * 1024;
const MIN_BLOCK: u64 = 1 << 20;
const MAX_BLOCK: u64 = 32 << 20;

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

fn probe(url: &str) -> Option<(f64, String, u64)> {
    let started = Instant::now();
    let mut resp = net::client()
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

fn pick_top(candidates: &[String], keep: usize) -> Vec<(String, u64)> {
    let results = Arc::new(Mutex::new(Vec::<(f64, String, u64)>::new()));
    let mut handles = Vec::new();
    for url in candidates.iter().take(4) {
        let url = url.clone();
        let results = Arc::clone(&results);
        handles.push(thread::spawn(move || {
            if let Some(info) = probe(&url) {
                results.lock().unwrap().push(info);
            }
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }
    let mut guard = results.lock().unwrap().clone();
    guard.retain(|(_, _, size)| *size > 0);
    guard.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    guard.into_iter().take(keep).map(|(_, url, size)| (url, size)).collect()
}

fn fetch_block(
    sources: &[String],
    file: &std::fs::File,
    start: u64,
    end: u64,
    progress: &Progress,
) -> Result<(), String> {
    let mut last = String::from("下载失败");
    for attempt in 0..6 {
        let url = &sources[attempt % sources.len().max(1)];
        if progress.cancel.load(Ordering::Relaxed) || progress.paused.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut resp = net::client()
            .get(url.as_str())
            .header("Referer", net::REFERER)
            .header("Range", format!("bytes={start}-{end}"))
            .send()
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            last = format!("HTTP {}", resp.status());
            thread::sleep(Duration::from_millis(500 * (attempt as u64 + 1)));
            continue;
        }
        let mut offset = start;
        let mut buf = vec![0u8; 256 * 1024];
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
                    break;
                }
            }
        }
        if offset == end + 1 {
            return Ok(());
        }
        last = "连接中断".into();
    }
    Err(last)
}

fn save_meta(path: &Path, meta: &Meta) {
    if let Ok(text) = serde_json::to_string(meta) {
        let _ = std::fs::write(path, text);
    }
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
                progress.total.store(dest.metadata().map(|m| m.len()).unwrap_or(0), Ordering::Relaxed);
                progress.done.store(progress.total.load(Ordering::Relaxed), Ordering::Relaxed);
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
    let top = pick_top(&pool, 2);
    let Some((best_url, total)) = top.first().cloned() else {
        return Err("所有下载线路都连接失败".into());
    };
    let sources: Vec<String> = top.iter().map(|(url, _)| url.clone()).collect();
    let meta_url = best_url.clone();
    progress.total.store(total, Ordering::Relaxed);
    *progress.host.lock().unwrap() = best_url
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let target = (conns as u64 * 10).clamp(1, 400);
    let block = (total / target).clamp(MIN_BLOCK, MAX_BLOCK);
    let blocks = total.div_ceil(block);

    let part = part_path(&dest);
    let meta_file = meta_path(&dest);

    // 读取断点信息
    let mut done_set: HashSet<u64> = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(&meta_file) {
        if let Ok(meta) = serde_json::from_str::<Meta>(&text) {
            if meta.size == total && meta.block == block && part.exists() {
                done_set = meta.done.into_iter().filter(|i| *i < blocks).collect();
            }
        }
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .open(&part)
        .map_err(|e| e.to_string())?;
    if file.metadata().map(|m| m.len()).unwrap_or(0) != total {
        file.set_len(total).map_err(|e| e.to_string())?;
        done_set.clear();
    }

    let baseline: u64 = done_set.iter().map(|i| (*i * block).min(total) - i * block).sum::<u64>()
        + done_set.iter().map(|i| (i * block + block).min(total) - i * block).sum::<u64>();
    let _ = baseline;
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

    progress.set_phase("下载中");
    let mut handles = Vec::new();
    for _ in 0..conns {
        let worker_meta = meta_url.clone();
        let cursor = Arc::clone(&cursor);
        let file = Arc::clone(&file);
        let done = Arc::clone(&done);
        let progress = Arc::clone(&progress);
        let saver = Arc::clone(&saver);
        let part = part.clone();
        let meta_file = meta_file.clone();
        let block_sources = sources.clone();
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
                fetch_block(&block_sources, &file, start, end, &progress)?;
                if progress.paused.load(Ordering::Relaxed) || progress.cancel.load(Ordering::Relaxed) {
                    return Ok(());
                }
                done.lock().unwrap().insert(index);
                let mut last = saver.lock().unwrap();
                if last.elapsed() > Duration::from_secs(2) {
                    let meta = Meta {
                        url: worker_meta.clone(),
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
        return Ok(Outcome::Paused);
    }

    if let Some(err) = error {
        return Err(err);
    }

    if let Some(expected) = &expected_md5 {
        progress.set_phase("校验 MD5");
        if !verify_md5(&part, expected)? {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(&meta_file);
            return Err("MD5 校验失败，文件可能损坏".into());
        }
    }

    std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&meta_file);
    progress.set_phase("已完成");
    Ok(Outcome::Done)
}
