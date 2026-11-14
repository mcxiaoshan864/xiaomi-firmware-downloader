#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod catalog;
mod config;
mod download;
mod net;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use catalog::{Device, Rom};
use config::Settings;
use download::{Outcome, Progress};
use windows_reactor::*;

const CARD: Color = Color::argb(26, 255, 255, 255);
const SHADE: Color = Color::argb(140, 0, 0, 0);

struct TaskItem {
    id: u64,
    title: String,
    urls: Vec<String>,
    detail: Option<String>,
    resolving: bool,
    method: String,
    md5: String,
    dest: PathBuf,
    progress: Arc<Progress>,
    speed: f64,
    last_done: u64,
    last_seen: Instant,
    finished: Option<Result<Outcome, String>>,
    started: bool,
    queued: bool,
    handle: Option<ComponentTask>,
}

struct AppState {
    devices: Vec<Device>,
    cached: HashMap<String, Vec<Rom>>,
    roms: Vec<Rom>,
    device_index: Option<usize>,
    rom_index: Option<usize>,
    device_view: Vec<usize>,
    rom_view: Vec<usize>,

    search: String,
    category: usize,
    category_labels: Vec<String>,
    region: String,
    method: String,
    branch: String,
    version_query: String,
    region_options: Vec<String>,
    branch_options: Vec<String>,

    tasks: Vec<TaskItem>,
    next_id: u64,
    settings: Settings,
    show_settings: bool,
    changelog: Option<(String, String)>,
    status: String,
    loading: bool,
    _loads: Vec<ComponentTask>,
    _tick: Option<ComponentTask>,
}

enum Msg {
    Loaded(Result<catalog::Catalog, String>),
    SelectDevice(Option<usize>),
    History(usize, Vec<Rom>),
    SelectRom(Option<usize>),
    Search(String),
    FilterCategory(Option<usize>),
    FilterRegion(Option<usize>),
    FilterMethod(Option<usize>),
    FilterBranch(Option<usize>),
    FilterVersion(String),
    Download,
    ToggleTask(u64),
    CancelTask(u64),
    RemoveTask(u64),
    OpenTask(u64),
    TaskDone(u64, Result<Outcome, String>),
    Resolved(u64, Vec<String>),
    Tick,
    CopyLink,
    OpenBrowser,
    ShowLog,
    OpenSettings,
    CloseSettings(bool),
    SetDir(String),
    SetConns(Option<f64>),
    SetMaxTasks(Option<f64>),
    CloseChangelog,
}

const CATEGORIES: [&str; 5] = ["小米", "Redmi", "POCO", "平板", "其他"];

fn category_of(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower.contains("poco") {
        "POCO"
    } else if lower.contains("pad") {
        "平板"
    } else if lower.starts_with("redmi") {
        "Redmi"
    } else if lower.starts_with("xiaomi") || lower.starts_with("mi ") || lower == "mi" {
        "小米"
    } else {
        "其他"
    }
}

fn category_counts(devices: &[Device]) -> [usize; 5] {
    let mut counts = [0usize; 5];
    for device in devices {
        if let Some(index) = CATEGORIES.iter().position(|c| *c == category_of(&device.name)) {
            counts[index] += 1;
        }
    }
    counts
}

fn filename_from_url(url: &str) -> String {
    url.rsplit('/').next().unwrap_or("firmware.bin").to_string()
}

impl TaskItem {
    fn active(&self) -> bool {
        self.started && self.finished.is_none()
    }
}

impl AppState {
    fn rebuild_devices(&mut self) {
        let query = self.search.to_lowercase();
        let category = if self.category == 0 {
            None
        } else {
            CATEGORIES.get(self.category - 1).copied()
        };
        self.device_view = self
            .devices
            .iter()
            .enumerate()
            .filter(|(_, device)| {
                let cat_ok = category.is_none_or(|c| category_of(&device.name) == c);
                let text_ok = query.is_empty()
                    || device.name.to_lowercase().contains(&query)
                    || device.base.to_lowercase().contains(&query);
                cat_ok && text_ok
            })
            .map(|(index, _)| index)
            .collect();
    }

    fn rebuild_roms(&mut self) {
        self.rom_view = self
            .roms
            .iter()
            .enumerate()
            .filter(|(_, rom)| {
                (self.region.is_empty() || rom.region == self.region)
                    && (self.method.is_empty() || rom.method == self.method)
                    && (self.branch.is_empty() || rom.branch == self.branch)
                    && (self.version_query.is_empty()
                        || rom.version.to_lowercase().contains(&self.version_query.to_lowercase()))
            })
            .map(|(index, _)| index)
            .collect();
    }

    fn refresh_options(&mut self) {
        let mut regions: Vec<String> = self
            .roms
            .iter()
            .map(|r| r.region.clone())
            .filter(|r| !r.is_empty())
            .collect();
        regions.sort();
        regions.dedup();
        self.region_options = regions;

        let mut branches: Vec<String> = self
            .roms
            .iter()
            .map(|r| r.branch.clone())
            .filter(|b| !b.is_empty())
            .collect();
        branches.sort();
        branches.dedup();
        self.branch_options = branches;
    }

    fn selected_rom(&self) -> Option<Rom> {
        self.rom_index.and_then(|index| self.roms.get(index)).cloned()
    }

    fn start_pending(&mut self, context: &ComponentContext<Self>) {
        let max_tasks = self.settings.max_tasks.max(1);
        let mut active = self.tasks.iter().filter(|t| t.active()).count();
        for index in 0..self.tasks.len() {
            if active >= max_tasks {
                break;
            }
            if self.tasks[index].started || self.tasks[index].finished.is_some() {
                continue;
            }
            if self.tasks[index].urls.is_empty() {
                continue;
            }
            if self.tasks[index].dest.as_os_str().is_empty() {
                let name = filename_from_url(&self.tasks[index].urls[0]);
                self.tasks[index].dest = PathBuf::from(&self.settings.download_dir).join(name);
            }
            let id = self.tasks[index].id;
            let urls = self.tasks[index].urls.clone();
            let dest = self.tasks[index].dest.clone();
            let md5 = if self.tasks[index].md5.is_empty() {
                None
            } else {
                Some(self.tasks[index].md5.clone())
            };
            let progress = Arc::clone(&self.tasks[index].progress);
            let conns = self.settings.connections.max(1);
            let handle = context.spawn_background(move |_token| {
                Msg::TaskDone(id, download::run(urls, dest, conns, md5, progress))
            });
            let task = &mut self.tasks[index];
            task.started = true;
            task.queued = false;
            task.progress.paused.store(false, Ordering::Relaxed);
            task.handle = Some(handle);
            active += 1;
        }
    }

    fn spawn_history(&mut self, index: usize, base: String, context: &ComponentContext<Self>) {
        let rss_base = base.clone();
        self._loads.push(context.spawn_background(move |_token| {
            Msg::History(index, catalog::load_history(&rss_base))
        }));
        self._loads.push(context.spawn_background(move |_token| {
            Msg::History(index, catalog::xiaomirom_history(&base))
        }));
    }

    fn queue_task(&mut self, context: &ComponentContext<Self>) {
        let Some(rom) = self.selected_rom() else {
            self.status = "请先在列表中选择一个固件版本".into();
            return;
        };
        let id = self.next_id;
        self.next_id += 1;
        let mut urls = Vec::new();
        if !rom.url.is_empty() {
            urls.push(rom.url.clone());
        }
        let resolving = urls.is_empty() && rom.detail.is_some();
        let dest = if urls.is_empty() {
            PathBuf::new()
        } else {
            PathBuf::from(&self.settings.download_dir).join(filename_from_url(&urls[0]))
        };
        self.tasks.push(TaskItem {
            id,
            title: format!("{} · {} · {}", rom.version, rom.region, rom.method),
            urls,
            detail: rom.detail.clone(),
            resolving,
            method: rom.method.clone(),
            md5: rom.md5.clone(),
            dest,
            progress: Progress::new(),
            speed: 0.0,
            last_done: 0,
            last_seen: Instant::now(),
            finished: None,
            started: false,
            queued: true,
            handle: None,
        });
        if resolving {
            self.status = format!("正在解析下载地址：{}", rom.version);
            if let Some(detail) = rom.detail.clone() {
                self._loads.push(context.spawn_background(move |_token| {
                    Msg::Resolved(id, catalog::resolve_download(&detail))
                }));
            }
        } else {
            self.status = format!("已加入队列：{}", rom.version);
        }
        self.start_pending(context);
        if self._tick.is_none() {
            self._tick = Some(context.spawn_background(|_token| {
                std::thread::sleep(Duration::from_millis(500));
                Msg::Tick
            }));
        }
    }

    fn task_view(&self, task: &TaskItem, context: &mut ViewContext<Self>) -> View {
        let done = task.progress.done.load(Ordering::Relaxed);
        let total = task.progress.total.load(Ordering::Relaxed);
        let percent = if total > 0 {
            done as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        let phase = if let Some(result) = &task.finished {
            match result {
                Ok(Outcome::Done) => "已完成".to_string(),
                Ok(Outcome::Cancelled) => "已取消".to_string(),
                Ok(Outcome::Paused) => "已暂停".to_string(),
                Err(err) => format!("失败：{err}"),
            }
        } else if task.progress.paused.load(Ordering::Relaxed) {
            "正在暂停…".to_string()
        } else if task.queued || !task.started {
            "排队中".to_string()
        } else {
            task.progress.phase()
        };

        let running = task.active() && !task.progress.paused.load(Ordering::Relaxed);
        let can_resume = matches!(task.finished, Some(Ok(Outcome::Paused)))
            || task.progress.paused.load(Ordering::Relaxed);
        let toggle_label = if can_resume { "继续" } else { "暂停" };
        let id = task.id;
        let toggle = Button::new()
            .is_enabled(can_resume || running)
            .on_click(context.callback(move |_: ()| Msg::ToggleTask(id)))
            .content(toggle_label);
        let cancel = Button::new()
            .is_enabled(task.finished.is_none())
            .on_click(context.callback(move |_: ()| Msg::CancelTask(id)))
            .content("取消");
        let open = Button::new()
            .on_click(context.callback(move |_: ()| Msg::OpenTask(id)))
            .content("打开");
        let remove = Button::new()
            .is_enabled(task.finished.is_some())
            .on_click(context.callback(move |_: ()| Msg::RemoveTask(id)))
            .content("移除");

        StackPanel::new().spacing(6.0).children((
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(10.0)
                .children((
                    TextBlock::new().text(task.title.clone()).font_weight(FontWeight::BOLD),
                    TextBlock::new().text(phase).font_size(12.0),
                )),
            ProgressBar::new().maximum(100.0).value(percent),
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(8.0)
                .children((
                    TextBlock::new().text(format!(
                        "{:.1}% · {:.1} MB/s · {} / {} · {}",
                        percent,
                        task.speed / 1024.0 / 1024.0,
                        human(done),
                        human(total),
                        task.progress.host()
                    )),
                    toggle,
                    cancel,
                    open,
                    remove,
                )),
        ))
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

impl Component for AppState {
    type Input = ();
    type Message = Msg;

    fn create(_input: &(), context: &ComponentContext<Self>) -> Self {
        let load = context.spawn_background(move |_token| match catalog::load() {
            Ok(data) => Msg::Loaded(Ok(data)),
            Err(err) => Msg::Loaded(Err(err)),
        });
        let tick = context.spawn_background(|_token| {
            std::thread::sleep(Duration::from_millis(500));
            Msg::Tick
        });
        Self {
            devices: Vec::new(),
            cached: HashMap::new(),
            roms: Vec::new(),
            device_index: None,
            rom_index: None,
            device_view: Vec::new(),
            rom_view: Vec::new(),
            search: String::new(),
            category: 0,
            category_labels: Vec::new(),
            region: String::new(),
            method: String::new(),
            branch: String::new(),
            version_query: String::new(),
            region_options: Vec::new(),
            branch_options: Vec::new(),
            tasks: Vec::new(),
            next_id: 1,
            settings: config::load(),
            show_settings: false,
            changelog: None,
            status: "正在获取固件目录…".into(),
            loading: true,
            _loads: vec![load],
            _tick: Some(tick),
        }
    }

    fn update(&mut self, message: Msg, context: &ComponentContext<Self>) {
        match message {
            Msg::Loaded(Ok(data)) => {
                self.loading = false;
                self.devices = data.devices;
                self.cached = data.roms;
                let counts = category_counts(&self.devices);
                let mut labels = vec!["全部分类".to_string()];
                for (index, name) in CATEGORIES.iter().enumerate() {
                    labels.push(format!("{name} ({})", counts[index]));
                }
                self.category_labels = labels;
                #[cfg(debug_assertions)]
                if let Ok(name) = std::env::var("MIFW_CAT") {
                    if let Some(index) = CATEGORIES.iter().position(|c| *c == name) {
                        self.category = index + 1;
                    }
                }
                self.rebuild_devices();
                self.status = format!("{} 款机型", self.devices.len());
                if let Some(first) = self.device_view.first().copied() {
                    self.device_index = Some(first);
                    let base = self.devices[first].base.clone();
                    self.roms = self.cached.get(&base).cloned().unwrap_or_default();
                    self.refresh_options();
                    self.rebuild_roms();
                    self.spawn_history(first, base.clone(), context);
                    #[cfg(debug_assertions)]
                    if std::env::var("MIFW_AUTODL").is_ok() {
                        if let Some(position) = self.rom_view.first().copied() {
                            self.rom_index = Some(position);
                            self.queue_task(context);
                        }
                    }
                }
            }
            Msg::Loaded(Err(err)) => {
                self.loading = false;
                self.status = format!("目录加载失败：{err}");
            }
            Msg::SelectDevice(position) => {
                let target = position.and_then(|p| self.device_view.get(p).copied());
                if target == self.device_index && !self.roms.is_empty() {
                    return;
                }
                self.device_index = target;
                self.rom_index = None;
                self.roms.clear();
                self.rom_view.clear();
                let Some(index) = self.device_index else { return };
                let device = self.devices[index].clone();
                self.roms = self.cached.get(&device.base).cloned().unwrap_or_default();
                self.region.clear();
                self.method.clear();
                self.branch.clear();
                self.version_query.clear();
                self.refresh_options();
                self.rebuild_roms();
                let load_index = index;
                self.spawn_history(load_index, device.base.clone(), context);
            }
            Msg::History(index, extra) => {
                if self.device_index != Some(index) || extra.is_empty() {
                    return;
                }
                let mut known: Vec<String> = self
                    .roms
                    .iter()
                    .filter(|r| !r.version.trim().is_empty())
                    .map(|r| format!("{}|{}|{}", r.version, r.region, r.method))
                    .collect();
                self.roms.retain(|r| !r.version.trim().is_empty());
                for rom in extra {
                    if rom.version.trim().is_empty() {
                        continue;
                    }
                    let key = format!("{}|{}|{}", rom.version, rom.region, rom.method);
                    if !known.contains(&key) {
                        known.push(key);
                        self.roms.push(rom);
                    }
                }
                self.roms.sort_by(|a, b| {
                    b.date.cmp(&a.date).then_with(|| b.version.cmp(&a.version))
                });
                self.refresh_options();
                self.rebuild_roms();
                self.status = format!("{} 个版本", self.roms.len());
            }
            Msg::SelectRom(position) => {
                self.rom_index = position.and_then(|p| self.rom_view.get(p).copied());
            }
            Msg::Search(text) => {
                self.search = text;
                self.rebuild_devices();
            }
            Msg::FilterCategory(position) => {
                self.category = position.unwrap_or(0);
                self.rebuild_devices();
            }
            Msg::FilterRegion(position) => {
                self.region = position
                    .and_then(|p| self.region_options.get(p.wrapping_sub(1)).cloned())
                    .unwrap_or_default();
                self.rebuild_roms();
            }
            Msg::FilterMethod(position) => {
                self.method = match position {
                    Some(1) => "卡刷".into(),
                    Some(2) => "线刷".into(),
                    _ => String::new(),
                };
                self.rebuild_roms();
            }
            Msg::FilterBranch(position) => {
                self.branch = position
                    .and_then(|p| self.branch_options.get(p.wrapping_sub(1)).cloned())
                    .unwrap_or_default();
                self.rebuild_roms();
            }
            Msg::FilterVersion(text) => {
                self.version_query = text;
                self.rebuild_roms();
            }
            Msg::Download => self.queue_task(context),
            Msg::Tick => {
                self._tick = Some(context.spawn_background(|_token| {
                    std::thread::sleep(Duration::from_millis(500));
                    Msg::Tick
                }));
                #[cfg(debug_assertions)]
                if std::env::var("MIFW_AUTODL2").is_ok()
                    && self.tasks.is_empty()
                    && self.roms.len() > 40
                {
                    if let Some(position) = self
                        .rom_view
                        .iter()
                        .rev()
                        .find(|i| self.roms[**i].url.is_empty() && self.roms[**i].detail.is_some())
                        .copied()
                    {
                        self.rom_index = Some(position);
                        self.queue_task(context);
                    }
                }
                for task in self.tasks.iter_mut() {
                    if task.active() {
                        let done = task.progress.done.load(Ordering::Relaxed);
                        let elapsed = task.last_seen.elapsed().as_secs_f64();
                        if elapsed > 0.2 {
                            task.speed = done.saturating_sub(task.last_done) as f64 / elapsed;
                            task.last_done = done;
                            task.last_seen = Instant::now();
                        }
                    } else {
                        task.speed = 0.0;
                    }
                }
            }
            Msg::ToggleTask(id) => {
                let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) else {
                    return;
                };
                if task.progress.paused.load(Ordering::Relaxed) {
                    task.progress.paused.store(false, Ordering::Relaxed);
                    task.finished = None;
                    task.started = false;
                    task.queued = true;
                    task.last_done = task.progress.done.load(Ordering::Relaxed);
                    task.last_seen = Instant::now();
                } else if task.finished.is_none() {
                    task.progress.paused.store(true, Ordering::Relaxed);
                }
                self.start_pending(context);
            }
            Msg::CancelTask(id) => {
                if let Some(task) = self.tasks.iter().find(|t| t.id == id) {
                    task.progress.cancel.store(true, Ordering::Relaxed);
                    task.progress.paused.store(false, Ordering::Relaxed);
                }
            }
            Msg::RemoveTask(id) => {
                self.tasks.retain(|t| t.id != id || t.finished.is_none());
                self.start_pending(context);
            }
            Msg::OpenTask(id) => {
                if let Some(task) = self.tasks.iter().find(|t| t.id == id) {
                    let folder = task
                        .dest
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_default();
                    let _ = std::fs::create_dir_all(&folder);
                    let _ = std::process::Command::new("explorer").arg(folder).spawn();
                }
            }
            Msg::TaskDone(id, result) => {
                let mut status = None;
                if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
                    status = Some(match &result {
                        Ok(Outcome::Done) => format!("下载完成：{}", task.title),
                        Ok(Outcome::Paused) => format!("已暂停：{}", task.title),
                        Ok(Outcome::Cancelled) => format!("已取消：{}", task.title),
                        Err(err) => format!("下载失败：{err}"),
                    });
                    task.finished = Some(result);
                }
                if let Some(text) = status {
                    self.status = text;
                }
                self.start_pending(context);
            }
            Msg::CopyLink => {
                if let Some(rom) = self.selected_rom() {
                    let _ = std::process::Command::new("cmd")
                        .args(["/c", "echo", &rom.url])
                        .spawn();
                    self.status = "链接已复制".into();
                }
            }
            Msg::OpenBrowser => {
                if let Some(rom) = self.selected_rom() {
                    let _ = std::process::Command::new("cmd")
                        .args(["/c", "start", "", &rom.url])
                        .spawn();
                }
            }
            Msg::ShowLog => {
                if let Some(rom) = self.selected_rom() {
                    let text = format!(
                        "机型：{}\n地区：{}\n类型：{}\n分支：{}\n安卓：{}\n大小：{}\nMD5：{}",
                        self.device_index
                            .and_then(|i| self.devices.get(i))
                            .map(|d| d.name.clone())
                            .unwrap_or_default(),
                        rom.region,
                        rom.method,
                        rom.branch,
                        rom.android,
                        rom.size_text,
                        if rom.md5.is_empty() { "未提供" } else { &rom.md5 }
                    );
                    self.changelog = Some((rom.version.clone(), text));
                }
            }
            Msg::Resolved(id, urls) => {
                if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
                    task.resolving = false;
                    let suffix = if task.method == "线刷" { ".tgz" } else { ".zip" };
                    let mut matched: Vec<String> =
                        urls.iter().filter(|u| u.contains(suffix)).cloned().collect();
                    if matched.is_empty() {
                        matched = urls;
                    }
                    let urls = matched;
                    if urls.is_empty() {
                        task.finished = Some(Err("无法解析下载地址".into()));
                        self.status = "解析下载地址失败，请换一个版本".into();
                    } else {
                        task.urls = urls;
                    }
                }
                self.start_pending(context);
            }
            Msg::OpenSettings => self.show_settings = true,
            Msg::CloseSettings(saved) => {
                if saved {
                    config::save(&self.settings);
                    self.status = "设置已保存".into();
                    self.start_pending(context);
                }
                self.show_settings = false;
            }
            Msg::SetDir(dir) => self.settings.download_dir = dir,
            Msg::SetConns(value) => {
                if let Some(value) = value {
                    self.settings.connections = value as usize;
                }
            }
            Msg::SetMaxTasks(value) => {
                if let Some(value) = value {
                    self.settings.max_tasks = value as usize;
                }
            }
            Msg::CloseChangelog => self.changelog = None,
        }
    }

    fn view(&self, _input: &(), context: &mut ViewContext<Self>) -> View {
        context.window_title("小米固件下载器");
        context.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .client_size(1120.0, 780.0),
        );

        let device_items: Vec<KeyedView> = self
            .device_view
            .iter()
            .enumerate()
            .map(|(position, index)| {
                let device = &self.devices[*index];
                KeyedView::new(
                    format!("d{index}"),
                    TextBlock::new().text(format!("{}   ·   {}", device.name, device.base)),
                )
                
            })
            .collect();

        let selected_position = self
            .device_index
            .and_then(|index| self.device_view.iter().position(|i| *i == index));

        let device_list = ListView::new()
            .selected_index(selected_position)
            .on_selection_changed(context.callback(Msg::SelectDevice))
            .collection_slot(ListViewSlot::Items, device_items);

        let search = TextBox::new()
            .placeholder_text("搜索机型 / 代号")
            .text(self.search.clone())
            .on_text_changed(context.callback(Msg::Search));

        let category_items = if self.category_labels.is_empty() {
            CATEGORIES
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
        } else {
            self.category_labels.clone()
        };
        let category_combo = ComboBox::new()
            .items_source(category_items)
            .selected_index(Some(self.category))
            .on_selection_changed(context.callback(Msg::FilterCategory));

        let left = Grid::new()
            .rows([
                GridLength::Auto,
                GridLength::Auto,
                GridLength::Auto,
                GridLength::STAR,
            ])
            .row_spacing(8.0)
            .children((
                TextBlock::new()
                    .text(format!("机型 · {}", self.device_view.len()))
                    .grid_row(0),
                Border::new().grid_row(1).content(category_combo),
                search.grid_row(2),
                Border::new().grid_row(3).content(device_list),
            ));

        let region_items: Vec<String> = std::iter::once("全部地区".to_string())
            .chain(self.region_options.iter().cloned())
            .collect();
        let region_selected = if self.region.is_empty() {
            Some(0)
        } else {
            self.region_options
                .iter()
                .position(|r| *r == self.region)
                .map(|p| p + 1)
        };
        let region_combo = ComboBox::new()
            .items_source(region_items)
            .selected_index(region_selected)
            .on_selection_changed(context.callback(Msg::FilterRegion));

        let method_combo = ComboBox::new()
            .items_source(vec!["全部类型".to_string(), "卡刷".into(), "线刷".into()])
            .selected_index(if self.method.is_empty() {
                Some(0)
            } else if self.method == "卡刷" {
                Some(1)
            } else {
                Some(2)
            })
            .on_selection_changed(context.callback(Msg::FilterMethod));

        let branch_items: Vec<String> = std::iter::once("全部分支".to_string())
            .chain(self.branch_options.iter().cloned())
            .collect();
        let branch_selected = if self.branch.is_empty() {
            Some(0)
        } else {
            self.branch_options
                .iter()
                .position(|b| *b == self.branch)
                .map(|p| p + 1)
        };
        let branch_combo = ComboBox::new()
            .items_source(branch_items)
            .selected_index(branch_selected)
            .on_selection_changed(context.callback(Msg::FilterBranch));

        let version_box = TextBox::new()
            .placeholder_text("筛选版本号")
            .text(self.version_query.clone())
            .on_text_changed(context.callback(Msg::FilterVersion));

        let filters = StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(8.0)
            .children((region_combo, method_combo, branch_combo, version_box));

        let rom_items: Vec<KeyedView> = self
            .rom_view
            .iter()
            .enumerate()
            .map(|(position, index)| {
                let rom = &self.roms[*index];
                KeyedView::new(
                    format!("r{index}"),
                    TextBlock::new().text(format!(
                        "{}   ·   {}   ·   {}   ·   {}   ·   {}   ·   {}",
                        rom.version, rom.region, rom.method, rom.branch, rom.size_text, rom.date
                    )),
                )
                
            })
            .collect();

        let rom_selected = self
            .rom_index
            .and_then(|index| self.rom_view.iter().position(|i| *i == index));

        let rom_list = ListView::new()
            .selected_index(rom_selected)
            .on_selection_changed(context.callback(Msg::SelectRom))
            .collection_slot(ListViewSlot::Items, rom_items);

        let selected_text = self
            .selected_rom()
            .map(|rom| {
                format!(
                    "{} · {} · {} · {}",
                    rom.version, rom.region, rom.method, rom.size_text
                )
            })
            .unwrap_or_else(|| "未选择版本".into());

        let download_bar = StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(10.0)
            .children((
                TextBlock::new()
                    .text(selected_text)
                    .font_weight(FontWeight::MEDIUM),
                Button::new()
                    .style(ButtonStyle::Accent)
                    .on_click(context.callback(|_: ()| Msg::Download))
                    .content("下载所选固件"),
                Button::new()
                    .on_click(context.callback(|_: ()| Msg::CopyLink))
                    .content("复制链接"),
                Button::new()
                    .on_click(context.callback(|_: ()| Msg::OpenBrowser))
                    .content("浏览器打开"),
                Button::new()
                    .on_click(context.callback(|_: ()| Msg::ShowLog))
                    .content("详情"),
            ));

        let rom_border = Border::new().grid_row(1).content(rom_list);
        let right = Grid::new()
            .rows([GridLength::Auto, GridLength::STAR, GridLength::Auto])
            .row_spacing(8.0)
            .children((
                Border::new().grid_row(0).content(filters),
                rom_border,
                Border::new().grid_row(2).content(download_bar),
            ));

        let header = Grid::new()
            .columns([GridLength::Auto, GridLength::STAR, GridLength::Auto])
            .children((
                Border::new().grid_column(0).content(
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .spacing(12.0)
                        .children((
                            TextBlock::new()
                                .text("小米固件下载器")
                                .font_size(24.0)
                                .font_weight(FontWeight::BOLD),
                            TextBlock::new().text(self.status.clone()).font_size(12.0),
                        )),
                ),
                Border::new().grid_column(2).content(
                    Button::new()
                        .on_click(context.callback(|_: ()| Msg::OpenSettings))
                        .content("设置"),
                ),
            ));

        let task_items: Vec<KeyedView> = self
            .tasks
            .iter()
            .map(|task| KeyedView::new(format!("t{}", task.id), self.task_view(task, context)))
            .collect();
        let tasks_body: View = if self.tasks.is_empty() {
            TextBlock::new()
                .text("暂无下载任务 · 选择版本后点「下载所选固件」")
                .font_size(12.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .into()
        } else {
            ListView::new()
                .collection_slot(ListViewSlot::Items, task_items)
                .into()
        };

        let tasks_panel = Grid::new()
            .rows([GridLength::Auto, GridLength::Pixel(170.0)])
            .row_spacing(6.0)
            .children((
                TextBlock::new()
                    .text("下载任务")
                    .font_size(16.0)
                    .grid_row(0),
                Border::new()
                    .grid_row(1)
                    .background(Brush::Solid(CARD))
                    .corner_radius(CornerRadius::uniform(8.0))
                    .padding(Thickness::uniform(8.0))
                    .content(tasks_body),
            ));

        let body = Grid::new()
            .columns([GridLength::Pixel(320.0), GridLength::STAR])
            .column_spacing(12.0)
            .children((
                Border::new()
                    .grid_column(0)
                    .background(Brush::Solid(CARD))
                    .corner_radius(CornerRadius::uniform(8.0))
                    .padding(Thickness::uniform(12.0))
                    .content(left),
                Border::new()
                    .grid_column(1)
                    .background(Brush::Solid(CARD))
                    .corner_radius(CornerRadius::uniform(8.0))
                    .padding(Thickness::uniform(12.0))
                    .content(right),
            ));

        let mut children: Vec<KeyedView> = vec![
            KeyedView::new("header", Border::new().grid_row(0).content(header)),
            KeyedView::new(
                format!("body-{}", self.device_index.unwrap_or(usize::MAX)),
                Border::new().grid_row(1).content(body),
            ),
            KeyedView::new("tasks", Border::new().grid_row(2).content(tasks_panel)),
        ];

        if self.show_settings {
            children.push(KeyedView::new("settings", self.settings_overlay(context)));
        }
        if let Some((title, text)) = &self.changelog {
            children.push(KeyedView::new(
                "changelog",
                self.changelog_overlay(title, text, context),
            ));
        }

        Border::new().padding(Thickness::uniform(16.0)).content(
            Grid::new()
                .rows([
                    GridLength::Auto,
                    GridLength::STAR,
                    GridLength::Auto,
                ])
                .row_spacing(12.0)
                .keyed_children(children),
        )
    }
}

impl AppState {
    fn settings_overlay(&self, context: &mut ViewContext<Self>) -> View {
        let content = StackPanel::new().spacing(10.0).children((
            TextBlock::new()
                .text("设置")
                .font_size(22.0)
                .font_weight(FontWeight::BOLD),
            TextBlock::new().text("下载目录"),
            TextBox::new()
                .text(self.settings.download_dir.clone())
                .on_text_changed(context.callback(Msg::SetDir)),
            TextBlock::new().text("每个任务的连接数（1–64）"),
            NumberBox::new()
                .value(self.settings.connections as f64)
                .minimum(1.0)
                .maximum(64.0)
                .on_value_changed(context.callback(Msg::SetConns)),
            TextBlock::new().text("同时下载的任务数（1–5）"),
            NumberBox::new()
                .value(self.settings.max_tasks as f64)
                .minimum(1.0)
                .maximum(5.0)
                .on_value_changed(context.callback(Msg::SetMaxTasks)),
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(8.0)
                .children((
                    Button::new()
                        .style(ButtonStyle::Accent)
                        .on_click(context.callback(|_: ()| Msg::CloseSettings(true)))
                        .content("保存"),
                    Button::new()
                        .on_click(context.callback(|_: ()| Msg::CloseSettings(false)))
                        .content("取消"),
                )),
        ));

        Grid::new()
            .grid_row_span(3)
            .background(Brush::Solid(SHADE))
            .children((Border::new()
                .width(520.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .background(Brush::Theme(ThemeBrush::SolidBackground))
                .border_brush(Brush::Theme(ThemeBrush::CardStroke))
                .border_thickness(Thickness::uniform(1.0))
                .corner_radius(CornerRadius::uniform(10.0))
                .padding(Thickness::uniform(20.0))
                .content(content),))
    }

    fn changelog_overlay(
        &self,
        title: &str,
        text: &str,
        context: &mut ViewContext<Self>,
    ) -> View {
        let content = Grid::new()
            .rows([GridLength::Auto, GridLength::STAR, GridLength::Auto])
            .row_spacing(10.0)
            .children((
                TextBlock::new()
                    .text(title)
                    .font_size(20.0)
                    .font_weight(FontWeight::BOLD)
                    .grid_row(0),
                Border::new()
                    .grid_row(1)
                    .content(ScrollViewer::new().content(
                        TextBlock::new().text(text).text_wrapping(TextWrapping::Wrap),
                    )),
                Border::new().grid_row(2).content(
                    Button::new()
                        .on_click(context.callback(|_: ()| Msg::CloseChangelog))
                        .content("关闭"),
                ),
            ));

        Grid::new()
            .grid_row_span(3)
            .background(Brush::Solid(SHADE))
            .children((Border::new()
                .width(560.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .background(Brush::Theme(ThemeBrush::SolidBackground))
                .border_brush(Brush::Theme(ThemeBrush::CardStroke))
                .border_thickness(Thickness::uniform(1.0))
                .corner_radius(CornerRadius::uniform(10.0))
                .padding(Thickness::uniform(20.0))
                .content(content),))
    }
}

/// 给 WinUI 窗口设置 exe 内嵌图标（任务栏 / 标题栏 / Alt-Tab）
mod winicon {
    use std::ffi::c_void;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn FindWindowW(class: *const u16, title: *const u16) -> isize;
        fn SendMessageW(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize;
        fn LoadImageW(inst: isize, name: *const u16, kind: u32, cx: i32, cy: i32, flags: u32) -> isize;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> isize;
    }

    const WM_SETICON: u32 = 0x0080;
    const IMAGE_ICON: u32 = 1;
    const LR_SHARED: u32 = 0x8000;
    const ICON_SMALL: usize = 0;
    const ICON_BIG: usize = 1;

    pub fn apply() {
        let title: Vec<u16> = "小米固件下载器\0".encode_utf16().collect();
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
            if hwnd == 0 {
                continue;
            }
            unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                let resource = 1usize as *const u16;
                let big = LoadImageW(module, resource, IMAGE_ICON, 32, 32, LR_SHARED);
                let small = LoadImageW(module, resource, IMAGE_ICON, 16, 16, LR_SHARED);
                if big != 0 {
                    SendMessageW(hwnd, WM_SETICON, ICON_BIG, big);
                }
                if small != 0 {
                    SendMessageW(hwnd, WM_SETICON, ICON_SMALL, small);
                }
            }
            return;
        }
    }
}

fn main() {
    std::thread::spawn(winicon::apply);
    windows_reactor::App::run_component::<AppState>(()).unwrap();
}
