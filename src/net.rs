//! HTTP helpers: Xiaomi CDN mirrors, Referer header, tracker data fetch.

use std::process::Command;
use std::sync::{Arc, OnceLock, RwLock};
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

type HttpClient = reqwest::blocking::Client;

fn build_client(proxy: Option<&str>) -> HttpClient {
    // With reqwest's blocking client this timeout applies to each streaming read.
    // A slow transfer can keep making progress, but a stalled read cannot delay a pause indefinitely.
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .redirect(redirect_policy())
        .user_agent(UA);
    if let Some(url) = proxy {
        if let Ok(proxy) = reqwest::Proxy::all(url) {
            builder = builder.proxy(proxy);
        }
    }
    builder.build().expect("http client")
}

/// Parse a firmware URL before letting it determine the file name or the download host.
pub fn firmware_url(raw: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || !CDN_HOSTS.contains(&url.host_str()?)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let name = url.path_segments()?.next_back()?;
    if !(name.ends_with(".zip") || name.ends_with(".tgz"))
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
    {
        return None;
    }
    let stem = name.split('.').next()?;
    let reserved = ["CON", "PRN", "AUX", "NUL"];
    if stem.is_empty()
        || reserved.iter().any(|word| stem.eq_ignore_ascii_case(word))
        || (stem.len() == 4
            && (stem[..3].eq_ignore_ascii_case("COM") || stem[..3].eq_ignore_ascii_case("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
    {
        return None;
    }
    Some(url)
}

pub fn firmware_filename(raw: &str) -> Option<String> {
    Some(firmware_url(raw)?.path_segments()?.next_back()?.to_string())
}

fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let previous = attempt.previous();
        if previous.len() >= 10 {
            return attempt.error("too many redirects");
        }
        if previous.first().is_some_and(|url| firmware_url(url.as_str()).is_some()) {
            let next = attempt.url();
            let last = &previous[previous.len() - 1];
            if firmware_url(next.as_str()).is_none()
                || next.path() != last.path()
                || (last.scheme() == "https" && next.scheme() != "https")
            {
                return attempt.error("固件链接重定向到了不同的地址");
            }
        }
        attempt.follow()
    })
}

static CLIENT: OnceLock<RwLock<Option<Arc<HttpClient>>>> = OnceLock::new();
static PROBE: OnceLock<RwLock<Option<Arc<HttpClient>>>> = OnceLock::new();

fn client_slot() -> &'static RwLock<Option<Arc<HttpClient>>> {
    CLIENT.get_or_init(|| RwLock::new(None))
}

fn probe_slot() -> &'static RwLock<Option<Arc<HttpClient>>> {
    PROBE.get_or_init(|| RwLock::new(None))
}

/// 按当前设置重建主客户端；proxy 为 None 时直连
pub fn configure(proxy: Option<String>) {
    let proxy = proxy.as_deref();
    let main = Arc::new(build_client(proxy));
    *client_slot().write().unwrap() = Some(main);
    let mut probe = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .connect_timeout(Duration::from_secs(4))
        .redirect(redirect_policy())
        .user_agent(UA);
    if let Some(url) = proxy {
        if let Ok(p) = reqwest::Proxy::all(url) {
            probe = probe.proxy(p);
        }
    }
    *probe_slot().write().unwrap() = Some(Arc::new(probe.build().expect("probe client")));
}

pub fn client() -> Arc<HttpClient> {
    if let Some(client) = client_slot().read().unwrap().as_ref() {
        return Arc::clone(client);
    }
    configure(None);
    Arc::clone(client_slot().read().unwrap().as_ref().expect("client"))
}

/// 读取 Windows「Internet 选项」里的代理设置（Clash / v2ray 开启系统代理时）
pub fn system_proxy() -> Option<String> {
    let enabled = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            "/v",
            "ProxyEnable",
        ])
        .output()
        .ok()?;
    if !String::from_utf8_lossy(&enabled.stdout).contains("0x1") {
        return None;
    }
    let server = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            "/v",
            "ProxyServer",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&server.stdout).to_string();
    let value = text.split("REG_SZ").nth(1)?.trim().to_string();
    if value.is_empty() {
        return None;
    }
    // 可能是 "host:port" 或 "http=host:port;https=host:port"
    let picked = value
        .split(';')
        .find_map(|part| part.strip_prefix("http="))
        .unwrap_or(&value)
        .trim()
        .to_string();
    Some(format!("http://{picked}"))
}

/// 测速专用客户端：整体 6 秒超时，避免慢节点拖住启动
pub fn probe_client() -> Arc<HttpClient> {
    if let Some(client) = probe_slot().read().unwrap().as_ref() {
        return Arc::clone(client);
    }
    configure(None);
    Arc::clone(probe_slot().read().unwrap().as_ref().expect("probe"))
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

/// 这些主机上 http(80) 实测明显快于 https（cdnorg: 4.6 vs 1.9 MB/s）
pub const HTTP_PREFERRED: &[&str] = &["cdnorg.d.miui.com", "bn.d.miui.com"];

fn host_of(url: &str) -> Option<&str> {
    let rest = &url[url.find("://")? + 3..];
    Some(rest.split('/').next().unwrap_or(rest))
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
    // 附加 http 变体：交给测速去挑，谁快用谁
    for candidate in out.clone() {
        let Some(host) = host_of(&candidate) else { continue };
        if HTTP_PREFERRED.contains(&host) {
            let http = candidate.replacen("https://", "http://", 1);
            if !out.contains(&http) {
                out.push(http);
            }
        }
    }
    out
}
