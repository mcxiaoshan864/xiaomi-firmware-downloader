//! 简单文件日志（exe 同级 logs/app.log），用于排查下载与崩溃问题。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

fn log_path() -> &'static PathBuf {
    LOG_PATH.get_or_init(|| {
        let mut dir = std::env::current_exe().unwrap_or_default();
        dir.pop();
        dir.push("logs");
        let _ = std::fs::create_dir_all(&dir);
        dir.push("app.log");
        // 超过 5MB 就清空，避免无限增长
        if let Ok(meta) = std::fs::metadata(&dir) {
            if meta.len() > 5 * 1024 * 1024 {
                let _ = std::fs::remove_file(&dir);
            }
        }
        dir
    })
}

#[repr(C)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

fn timestamp() -> String {
    unsafe extern "system" {
        fn GetLocalTime(value: *mut SystemTime);
    }
    let mut value = SystemTime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    unsafe { GetLocalTime(&mut value) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        value.year,
        value.month,
        value.day,
        value.hour,
        value.minute,
        value.second,
        value.milliseconds
    )
}

pub fn log(message: &str) {
    let path = log_path();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "[{}] {}", timestamp(), message);
    }
}

pub fn init() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        log(&format!("PANIC at {location}: {info}"));
    }));
    log("================ app start ================");
}

#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {
        $crate::logger::log(&format!($($arg)*))
    };
}
