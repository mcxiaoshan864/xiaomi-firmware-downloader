//! 机型 / 版本 / 历史版本解析（数据来自 XiaomiFirmwareUpdater 追踪库）。

use std::collections::HashMap;
use std::path::PathBuf;

use crate::net;

#[derive(Clone, Debug)]
pub struct Device {
    pub base: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Rom {
    pub version: String,
    pub region: String,
    pub method: String,
    pub branch: String,
    pub android: String,
    pub size_text: String,
    pub date: String,
    pub url: String,
    pub md5: String,
    pub detail: Option<String>,
}

pub struct Catalog {
    pub devices: Vec<Device>,
    pub roms: HashMap<String, Vec<Rom>>,
}

const REGIONS: &[(&str, &str)] = &[
    ("Latin America", "拉美"),
    ("South Africa", "南非"),
    ("Saudi Arabia", "沙特"),
    ("Hong Kong", "香港"),
    ("Singapore", "新加坡"),
    ("Indonesia", "印尼"),
    ("Philippines", "菲律宾"),
    ("Malaysia", "马来西亚"),
    ("Thailand", "泰国"),
    ("Vietnam", "越南"),
    ("Pakistan", "巴基斯坦"),
    ("Bangladesh", "孟加拉"),
    ("Kazakhstan", "哈萨克斯坦"),
    ("Ukraine", "乌克兰"),
    ("Turkey", "土耳其"),
    ("Russia", "俄罗斯"),
    ("Taiwan", "台湾"),
    ("Japan", "日本"),
    ("India", "印度"),
    ("Korea", "韩国"),
    ("Brazil", "巴西"),
    ("Mexico", "墨西哥"),
    ("Poland", "波兰"),
    ("Spain", "西班牙"),
    ("Italy", "意大利"),
    ("Germany", "德国"),
    ("France", "法国"),
    ("Macau", "澳门"),
    ("Global", "国际"),
    ("Europe", "欧洲"),
    ("EEA", "欧洲"),
    ("China", "中国"),
    ("Egypt", "埃及"),
    ("Israel", "以色列"),
    ("UAE", "阿联酋"),
];

fn split_region(name: &str) -> (String, String) {
    for (en, cn) in REGIONS {
        let suffix = format!(" {en}");
        if let Some(stripped) = name.strip_suffix(&suffix) {
            return (stripped.trim().to_string(), cn.to_string());
        }
    }
    (name.trim().to_string(), String::new())
}

fn base_codename(code: &str) -> String {
    let mut s = code.to_string();
    if let Some(stripped) = s.strip_suffix("_global") {
        s = stripped.to_string();
        if let Some(index) = s.rfind('_') {
            let tail = &s[index + 1..];
            if tail.len() == 2 || tail.len() == 3 {
                s.truncate(index);
            }
        }
    }
    s
}

/// devices.yml: `code:` 后跟 `- 名称` `- 型号`
fn parse_devices(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if !trimmed.starts_with('-') && trimmed.ends_with(':') && !trimmed.starts_with(' ') {
            let code = trimmed.trim_end_matches(':').trim().to_string();
            if !code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                current = Some(code);
            }
            continue;
        }
        if let Some(code) = &current {
            if let Some(value) = trimmed.strip_prefix("- ") {
                let value = value.trim().trim_matches(|c| c == '\'' || c == '"');
                if !value.is_empty() {
                    out.entry(code.clone()).or_insert_with(|| value.to_string());
                }
            }
        }
    }
    out
}

fn unquote(value: &str) -> String {
    value.trim().trim_matches(|c| c == '\'' || c == '"').to_string()
}

fn method_cn(method: &str) -> String {
    match method.trim() {
        "Recovery" => "卡刷".into(),
        "Fastboot" => "线刷".into(),
        other => other.into(),
    }
}

fn branch_cn(branch: &str) -> String {
    match branch.trim() {
        "Stable" => "稳定版".into(),
        "Stable Beta" => "稳定内测".into(),
        "Public Beta" => "公测版".into(),
        "Weekly" => "开发版".into(),
        "Beta" => "公测版".into(),
        other => other.into(),
    }
}

fn parse_latest(text: &str) -> Vec<(String, String, Rom)> {
    let mut items: Vec<HashMap<String, String>> = Vec::new();
    let mut current: Option<HashMap<String, String>> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("- ") {
            if let Some(item) = current.take() {
                items.push(item);
            }
            let mut map = HashMap::new();
            if let Some((key, value)) = rest.split_once(": ") {
                map.insert(key.trim().to_string(), value.trim().to_string());
            }
            current = Some(map);
        } else if let Some(item) = current.as_mut() {
            if let Some((key, value)) = line.trim().split_once(": ") {
                item.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }
    if let Some(item) = current {
        items.push(item);
    }

    let mut out = Vec::new();
    for mut item in items {
        let url = unquote(item.get("link").map(String::as_str).unwrap_or(""));
        if url.is_empty() {
            continue;
        }
        let name = unquote(item.get("name").map(String::as_str).unwrap_or(""));
        let (_, region) = split_region(&name);
        let md5 = unquote(item.get("md5").map(String::as_str).unwrap_or(""));
        let rom = Rom {
            version: unquote(item.get("version").map(String::as_str).unwrap_or("")),
            region,
            method: method_cn(&unquote(item.get("method").map(String::as_str).unwrap_or(""))),
            branch: branch_cn(&unquote(item.get("branch").map(String::as_str).unwrap_or(""))),
            android: unquote(item.get("android").map(String::as_str).unwrap_or("")),
            size_text: unquote(item.get("size").map(String::as_str).unwrap_or("")),
            date: unquote(item.get("date").map(String::as_str).unwrap_or("")),
            url,
            md5: if md5.eq_ignore_ascii_case("none") { String::new() } else { md5 },
            detail: None,
        };
        let code = item.remove("codename").unwrap_or_default();
        let code = unquote(&code);
        out.push((code, name, rom));
    }
    out
}

fn unescape_html(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push('\n');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn field(fields: &HashMap<String, String>, key: &str) -> String {
    fields.get(key).cloned().unwrap_or_default()
}

/// RSS：每个机型的历史版本
pub fn parse_rss(text: &str) -> Vec<Rom> {
    let mut roms = Vec::new();
    for chunk in text.split("<item>").skip(1) {
        let item = chunk.split("</item>").next().unwrap_or("");
        let between = |tag: &str| -> String {
            item.split(&format!("<{tag}>"))
                .nth(1)
                .and_then(|rest| rest.split(&format!("</{tag}>")).next())
                .map(unescape_html)
                .unwrap_or_default()
        };
        let title = between("title");
        let link = between("link").trim().to_string();
        let description = between("description");
        if link.is_empty() {
            continue;
        }

        let cleaned = strip_tags(&description);
        let mut fields: HashMap<String, String> = HashMap::new();
        let mut changelog: Vec<String> = Vec::new();
        let mut in_changelog = false;
        for raw in cleaned.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                if matches!(
                    key,
                    "Device" | "Codename" | "Version" | "Size" | "MD5" | "Changelog" | "Download"
                ) {
                    if key == "Changelog" {
                        in_changelog = true;
                        if !value.trim().is_empty() {
                            changelog.push(value.trim().to_string());
                        }
                        fields.insert(key.to_string(), String::new());
                        continue;
                    }
                    in_changelog = false;
                    fields.insert(key.to_string(), value.trim().to_string());
                    continue;
                }
            }
            if in_changelog {
                changelog.push(line.to_string());
            }
        }

        let version_field = field(&fields, "Version");
        let (version, android) = version_field
            .split_once('|')
            .map(|(v, a)| (v.trim().to_string(), a.trim().to_string()))
            .unwrap_or_else(|| (version_field.trim().to_string(), String::new()));

        let device = field(&fields, "Device");
        let (_, region) = split_region(&device);
        let method = if title.to_lowercase().contains("fastboot") {
            "线刷"
        } else {
            "卡刷"
        };
        let branch = if title.contains("Public Beta") {
            "公测版"
        } else if title.contains("Stable Beta") {
            "稳定内测"
        } else if version.contains(".DEV") {
            "公测版"
        } else if version.starts_with('V') || version.starts_with("OS") {
            "稳定版"
        } else {
            "开发版"
        };
        let md5 = field(&fields, "MD5");
        roms.push(Rom {
            version,
            region,
            method: method.to_string(),
            branch: branch.to_string(),
            android,
            size_text: field(&fields, "Size"),
            date: normalize_date(&between("pubDate")),
            url: link,
            md5: if md5.eq_ignore_ascii_case("none") { String::new() } else { md5 },
            detail: None,
        });
    }
    roms
}

fn cache_dir() -> PathBuf {
    let mut path = std::env::current_exe().unwrap_or_default();
    path.pop();
    path.push("cache");
    path
}

/// 先读本地缓存，缓存没有再去拉取；拉取成功写回缓存（断网也能用）
fn cached(remote: &str) -> String {
    let file = cache_dir().join(remote.replace('/', "_"));
    if let Ok(text) = std::fs::read_to_string(&file) {
        if text.len() > 32 {
            return text;
        }
    }
    let text = net::fetch_tracker(remote).unwrap_or_default();
    if text.len() > 32 {
        let _ = std::fs::create_dir_all(cache_dir());
        let _ = std::fs::write(&file, &text);
    }
    text
}

pub fn load() -> Result<Catalog, String> {
    let devices_text = cached("data/devices.yml");
    let latest_text = cached("data/latest.yml");
    if devices_text.is_empty() || latest_text.is_empty() {
        return Err("无法获取固件目录，请检查网络".into());
    }
    let names = parse_devices(&devices_text);
    let entries = parse_latest(&latest_text);

    let mut by_base: HashMap<String, Vec<Rom>> = HashMap::new();
    let mut display: HashMap<String, String> = HashMap::new();
    for (code, name, rom) in entries {
        let base = base_codename(&code);
        if base.is_empty() {
            continue;
        }
        by_base.entry(base.clone()).or_default().push(rom);
        let (clean, _) = split_region(&name);
        display.entry(base).or_insert(clean);
    }

    let mut devices: Vec<Device> = by_base
        .keys()
        .map(|base| {
            let name = names
                .get(base)
                .map(|n| split_region(n).0)
                .filter(|n| !n.is_empty())
                .or_else(|| display.get(base).cloned())
                .unwrap_or_else(|| base.clone());
            Device { base: base.clone(), name }
        })
        .collect();
    devices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    for roms in by_base.values_mut() {
        roms.sort_by(|a, b| b.version.cmp(&a.version));
    }

    Ok(Catalog { devices, roms: by_base })
}

pub fn load_history(base: &str) -> Vec<Rom> {
    let text = cached(&format!("rss/{base}.xml"));
    if text.is_empty() {
        return Vec::new();
    }
    parse_rss(&text)
}

// ---------------------------------------------------------------- xiaomirom.com

const XIAOMIROM_REGIONS: &[(&str, &str)] = &[
    ("russia", "俄罗斯"),
    ("global", "国际"),
    ("europe", "欧洲"),
    ("eea", "欧洲"),
    ("china", "中国"),
    ("india", "印度"),
    ("indonesia", "印尼"),
    ("taiwan", "台湾"),
    ("turkey", "土耳其"),
    ("japan", "日本"),
    ("korea", "韩国"),
    ("singapore", "新加坡"),
    ("malaysia", "马来西亚"),
    ("thailand", "泰国"),
    ("vietnam", "越南"),
    ("philippines", "菲律宾"),
    ("pakistan", "巴基斯坦"),
    ("bangladesh", "孟加拉"),
    ("ukraine", "乌克兰"),
    ("kazakhstan", "哈萨克斯坦"),
    ("egypt", "埃及"),
    ("morocco", "摩洛哥"),
    ("south-africa", "南非"),
    ("latin-america", "拉美"),
    ("brazil", "巴西"),
    ("mexico", "墨西哥"),
    ("uae", "阿联酋"),
    ("saudi-arabia", "沙特"),
    ("israel", "以色列"),
    ("poland", "波兰"),
    ("spain", "西班牙"),
    ("italy", "意大利"),
    ("germany", "德国"),
    ("france", "法国"),
    ("uk", "英国"),
    ("hong-kong", "香港"),
    ("macau", "澳门"),
    ("nepal", "尼泊尔"),
    ("sri-lanka", "斯里兰卡"),
];

fn region_from_url(url: &str) -> String {
    let slug = url.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let core = slug.strip_suffix("-fastboot-recovery-rom").unwrap_or(slug);
    let token = core.rsplit('-').next().unwrap_or("");
    for (key, cn) in XIAOMIROM_REGIONS {
        if *key == token {
            return (*cn).to_string();
        }
    }
    String::new()
}

fn normalize_date(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 10 {
        let head = &value[..10];
        let bytes = head.as_bytes();
        if bytes[4] == b'-' && bytes[7] == b'-' {
            return head.to_string();
        }
    }
    // "Fri, 10 Nov 2026 00:00:00 +0000"
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() >= 4 {
        let day = parts[1];
        let month = match parts[2] {
            "Jan" => "01", "Feb" => "02", "Mar" => "03", "Apr" => "04",
            "May" => "05", "Jun" => "06", "Jul" => "07", "Aug" => "08",
            "Sep" => "09", "Oct" => "10", "Nov" => "11", "Dec" => "12",
            _ => "",
        };
        if !month.is_empty() {
            return format!("{}-{}-{:0>2}", parts[3], month, day);
        }
    }
    value.to_string()
}

fn branch_from_version(version: &str) -> String {
    if version.contains(".DEV") {
        "公测版".into()
    } else if version.starts_with('V') || version.starts_with("OS") {
        "稳定版".into()
    } else {
        "开发版".into()
    }
}

fn parse_region_page(html: &str, page_url: &str) -> Vec<Rom> {
    let row_re = regex::Regex::new(
        r"<tr><td align=left>(.*?)</td><td align=left>(.*?)</td><td align=left>(.*?)</td><td align=left>(.*?)</td><td align=right>(.*?)</td><td align=left>(.*?)</td><td align=left>(.*?)</td></tr>",
    )
    .unwrap();
    let link_re = regex::Regex::new(r#"href=(/download/[^ >"]+)"#).unwrap();
    let region = region_from_url(page_url);

    let mut roms = Vec::new();
    for chunk in html.split("<h3 id=").skip(1) {
        let Some(row) = row_re.captures(chunk) else { continue };
        let method_cell = row.get(3).map(|m| m.as_str()).unwrap_or("");
        if !method_cell.contains("Fastboot") && !method_cell.contains("Recovery") {
            continue;
        }
        let version = row.get(4).map(|m| m.as_str().trim()).unwrap_or("").to_string();
        if version.is_empty() {
            continue;
        }
        let method = if method_cell.contains("Fastboot") { "线刷" } else { "卡刷" };
        let detail = link_re
            .captures(chunk)
            .and_then(|c| c.get(1))
            .map(|m| format!("https://xiaomirom.com{}", m.as_str().split('#').next().unwrap_or("")));
        roms.push(Rom {
            version: version.clone(),
            region: region.clone(),
            method: method.to_string(),
            branch: branch_from_version(&version),
            android: row.get(5).map(|m| m.as_str().trim()).unwrap_or("").to_string(),
            size_text: row.get(6).map(|m| m.as_str().trim()).unwrap_or("").to_string(),
            date: row.get(7).map(|m| m.as_str().trim()).unwrap_or("").to_string(),
            url: String::new(),
            md5: String::new(),
            detail,
        });
    }
    roms
}

/// 从 xiaomirom.com 抓取某个代号的全部历史版本（按地区页并发抓取）

pub fn xiaomirom_history(base: &str) -> Vec<Rom> {
    let hub = format!("https://xiaomirom.com/series/{base}/");
    let Ok(html) = net::fetch_text(&hub) else {
        return Vec::new();
    };
    let re = regex::Regex::new(r#"href="?(https://xiaomirom\.com/rom/[^"\s>]+)"#).unwrap();
    let mut pages: Vec<String> = re
        .captures_iter(&html)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect();
    pages.sort();
    pages.dedup();
    let pages_len = pages.len();
    if pages.is_empty() {
        return Vec::new();
    }

    let mut handles = Vec::new();
    for page in pages {
        handles.push(std::thread::spawn(move || {
            let html = net::fetch_text(&page).ok()?;
            Some(parse_region_page(&html, &page))
        }));
    }
    let mut out = Vec::new();
    for handle in handles {
        if let Ok(Some(mut roms)) = handle.join() {
            out.append(&mut roms);
        }
    }
    out
}

/// 解析下载页，取出所有官方 CDN 镜像地址
pub fn resolve_download(detail: &str) -> Vec<String> {
    let Ok(html) = net::fetch_text(detail) else {
        return Vec::new();
    };
    let re = regex::Regex::new(
        r#"https://(?:bigota|cdnorg|hugeota|bn)\.d\.miui\.com/[^"'\s<]+|https://bkt-sgp-[^"'\s<]+"#,
    )
    .unwrap();
    let mut urls: Vec<String> = re
        .find_iter(&html)
        .map(|m| m.as_str().to_string())
        .filter(|u| u.contains(".zip") || u.contains(".tgz"))
        .collect();
    urls.sort();
    urls.dedup();
    urls
}
