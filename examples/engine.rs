#[path = "../src/download.rs"]
mod download;
#[path = "../src/logger.rs"]
mod logger;
#[path = "../src/net.rs"]
mod net;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() {
    logger::init();
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("full");
    let url = args.get(2).cloned().unwrap_or_default();
    let dest = std::path::PathBuf::from(args.get(3).cloned().unwrap_or_else(|| "out.bin".into()));
    let md5 = args.get(4).cloned().filter(|m| !m.is_empty());
    let progress = download::Progress::new();
    let worker_progress = Arc::clone(&progress);

    if mode == "pause" || mode == "cancel" {
        let p = Arc::clone(&progress);
        let is_cancel = mode == "cancel";
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            if is_cancel {
                p.cancel.store(true, Ordering::Relaxed);
            } else {
                p.paused.store(true, Ordering::Relaxed);
            }
        });
    }

    let conns = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(16);
    let result = download::run(vec![url], dest.clone(), conns, md5.clone(), worker_progress);
    println!("RESULT {:?}", result);
    println!("DONE_BYTES {}", progress.done.load(Ordering::Relaxed));
}
