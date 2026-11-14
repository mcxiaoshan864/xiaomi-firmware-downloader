//! 设置持久化（放在 exe 旁边的 config.json）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub download_dir: String,
    pub connections: usize,
    pub max_tasks: usize,
    /// 走 Windows 系统代理（Clash / v2ray 等），默认关闭
    #[serde(default)]
    pub use_system_proxy: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            connections: 32,
            max_tasks: 2,
            use_system_proxy: false,
        }
    }
}

pub fn exe_dir() -> PathBuf {
    let mut path = std::env::current_exe().unwrap_or_default();
    path.pop();
    path
}

pub fn config_path() -> PathBuf {
    exe_dir().join("config.json")
}

/// 默认下载到剩余空间最大的非系统盘（优先 D 盘）
pub fn default_download_dir() -> String {
    if std::path::Path::new("D:\\").exists() {
        return "D:\\XiaomiFirmware".to_string();
    }
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    format!("{home}\\Downloads\\XiaomiFirmware")
}

pub fn load() -> Settings {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) {
    if let Ok(text) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(config_path(), text);
    }
}
