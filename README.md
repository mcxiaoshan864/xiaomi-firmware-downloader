# 小米固件下载器 · MiFirmware Downloader

> 用 Rust + WinUI 3 写的 Windows 小米 / Redmi / POCO 固件下载工具：多线路测速、多线程分段、断点续传、全量历史版本。

A native Windows firmware downloader for Xiaomi / Redmi / POCO devices, built with Rust and WinUI 3
(via [windows-rs / windows-reactor](https://github.com/microsoft/windows-rs)). Multi-CDN speed testing,
segmented parallel downloads, resumable transfers, and full ROM history.

![界面截图](docs/screenshot.png)

## 功能

- **双数据源合并**：XiaomiFirmwareUpdater 追踪库（最新版本、MD5、直链）+ xiaomirom.com（每机型全量历史版本，按需解析直链）
- **机型分类**：小米 / Redmi / POCO / 平板 / 其他，带数量，支持名称、代号实时搜索
- **版本筛选**：地区 / 卡刷·线刷 / 分支 + 版本号搜索，按日期倒序
- **线路择优**：小米 CDN 的 80 端口通常比 443 更快，测速会优先挑 HTTP 线路；直连很慢时可在设置里打开「使用 Windows 系统代理」，借用 Clash / v2ray 的出口
- **下载加速**：5 条官方 CDN（bigota / cdnorg / hugeota / bn.d.miui.com / 阿里云 OSS）并发测速，取最快 2 条并行分段，默认 16 线程（可调 1–64），不限速
- **断点续传**：暂停、退出、关机后继续；块级进度持久化
- **MD5 校验**：有 MD5 的来源下载完自动核对
- **多任务队列**：默认同时 2 个（可调 1–5）
- **设置**：下载目录、连接数、并发数，存于 exe 旁 `config.json`
- **原生体验**：Mica 背景、系统深浅色、标题栏/任务栏图标、单文件 exe（约 6 MB）

## 下载与运行

从 [Releases](../../releases) 下载 `小米固件下载器.exe`（或自行构建），双击运行。

需要系统已安装 **Windows App SDK 运行时**（Windows 11 一般自带；也可装
`Microsoft.WindowsAppRuntime.1.7` 及以上）。

默认下载目录为 `D:\XiaomiFirmware`（可在设置中修改）。

## 构建

需要 Rust 1.95+ 与 MSVC 工具链（`rustc`/`cargo` + Windows SDK 的 `rc.exe`）。

```bash
cargo build --release
# 产物：target/release/mifw_winui.exe
```

CI：GitHub Actions 会在 Windows runner 上自动构建并上传 artifact（见 `.github/workflows/build.yml`）。

## 项目结构

```
src/
├─ main.rs       Reactor 组件：界面、状态、消息
├─ catalog.rs    XiaomiFirmwareUpdater + xiaomirom 解析与缓存
├─ download.rs   多线路测速、双线路并行分段、续传、MD5
├─ config.rs     设置持久化
└─ net.rs        HTTP / CDN 候选 / Referer
tools/gen_icon.py  图标生成脚本（Pillow）
```

## 实现要点

- 小米 CDN 对无 `Referer` 的请求返回 403，所有请求都带 `Referer: https://www.mi.com/`
- 下载前对候选线路各拉 512 KB 样本测速，选最快两条，块级轮换使用
- xiaomirom 历史版本按 `机型 → 地区页 → 版本行` 解析；下载时才解析该版本的官方直链，避免预抓
- WinUI 3 使用 [windows-reactor](https://crates.io/crates/windows-reactor) 声明式 API，无 XAML 文件

## 数据来源与免责声明

- 版本数据来自开源项目 [XiaomiFirmwareUpdater](https://github.com/XiaomiFirmwareUpdater/miui-updates-tracker) 与第三方站点 xiaomirom.com 的公开页面；本项目只解析**公开可访问的页面**，运行时会以较低频率请求，请遵守对应站点的使用条款
- 所有下载链接均指向**小米官方 CDN**，本项目不托管、不分发任何固件，也不修改固件
- 与小米公司无任何关联；Xiaomi、Redmi、POCO、MIUI、HyperOS 等商标归各自所有者
- 刷机有风险，请自行核对机型与版本，后果自负

## 路线图

- [ ] 下载器：动态分块重平衡、块级换线重试
- [ ] 多选批量下载、完成通知、收藏机型
- [ ] 代理设置、拼音搜索
- [ ] 安装包（MSIX / Inno Setup）与自动更新
- [ ] 解析器与续传逻辑的单元测试

## License

[MIT](LICENSE)

## 更新日志

### v0.2.0

- 修复慢线路上「下载速度几乎为 0」：原来 30 秒总超时 + 64 MB 大块，慢线路每块必然超时、无限重试；改为 600 秒超时 + 1–8 MB 小块
- 测速最多等 4 秒，不再卡住启动；失败时按 cdnorg → bn → 阿里云 → bigota 顺序兜底
- 块级多线路轮询 + 线路健康度降级、指数退避，单条线路抖动不再拖垮整体
- 实测小米 CDN 的 HTTP(80) 明显快于 HTTPS（cdnorg 4.6 vs 1.9 MB/s），候选线路加入 HTTP 变体由测速自动择优
- 默认连接数 16 → 32
- 新增「使用 Windows 系统代理」开关（读系统代理设置，可借用 Clash / v2ray 出口）
- `.meta` 改为原子写，断电不再写坏；下载前预检磁盘空间；新增 `logs/app.log` 日志
