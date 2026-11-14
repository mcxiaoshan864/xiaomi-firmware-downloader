//! HTTP helpers: Xiaomi CDN mirrors, Referer header, tracker data fetch.

use std::sync::OnceLock;
use std::time::Duration;

pub const REFERER: &str = "https://www.mi.com/";
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// 同一份固件在小米官方各 CDN 上的主机名
pub const CDN_HOSTS: &[&str] = &[
    "bigota.d.miui.com",
    "cdnorg.d.miui.com",
    "hugeota.d.miui.com",
    "bn.d.miui.com",
    "bkt-sgp-miui-ota-update-alisgp.oss-ap-southeast-1.aliyuncs.com",
];

/// 版本数据源
const TRACKER_MIRRORS: &[&str] = &[
    "https://cdn.jsdelivr.net/gh/XiaomiFirmwareUpdater/miui-updates-tracker@master/",
    "https://fastly.jsdelivr.net/gh/XiaomiFirmwareUpdater/miui-updates-tracker@master/",
    "https://raw.githubusercontent.com/XiaomiFirmwareUpdater/miui-updates-tracker/master/",
];

pub fn client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(UA)
            .build()
            .expect("http client")
    })
}

pub fn fetch_text(url: &str) -> Result<String, String> {
    let resp = client()
        .get(url)
        .header("Referer", REFERER)
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.text().map_err(|e| e.to_string())
}

pub fn fetch_tracker(path: &str) -> Result<String, String> {
    let mut last = String::from("没有可用的数据源");
    for mirror in TRACKER_MIRRORS {
        let url = format!("{mirror}{path}");
        match fetch_text(&url) {
            Ok(text) if text.len() > 32 => return Ok(text),
            Ok(_) => continue,
            Err(err) => last = err,
        }
    }
    Err(last)
}

/// 从任意 CDN 链接中取出路径部分（含文件名）
pub fn url_path(url: &str) -> Option<String> {
    let scheme = url.find("://")?;
    let rest = &url[scheme + 3..];
    let path_start = rest.find('/')?;
    Some(rest[path_start..].to_string())
}

/// 同一路径在各 CDN 上的候选地址（第一个是原始链接）
pub fn mirror_candidates(url: &str) -> Vec<String> {
    let Some(path) = url_path(url) else {
        return vec![url.to_string()];
    };
    let mut out = vec![url.to_string()];
    for host in CDN_HOSTS {
        let candidate = format!("https://{host}{path}");
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}
