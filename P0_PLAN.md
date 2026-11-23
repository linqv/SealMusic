# Zeedle Fork — P0 裁剪、重构与 MPRIS2 桌面集成计划

> 注：本项目现已更名为 **SealMusic**，仓库地址为 <https://github.com/linqv/SealMusic>。以下记录保留更名前（`zeedlelocal`）的原始标识，作为历史参考。
>
> 状态：P0 已完成（2026-11-23）  
> 基线：Zeedle 上游 `main`，检查时 `Cargo.toml` 标识版本 `0.7.2`（2026-11-23）  
> 上游仓库：<https://github.com/Jordan-Haidee/zeedle>  
> 优先平台：Linux / Wayland（Arch Linux、niri）；源码保留 Windows/macOS `#[cfg(...)]` 条件编译边界，构建与验证严格仅针对 Linux（不打包、不构建其他平台）

## 1. 目标与边界

P0 只解决四件事：**删除非核心功能、精简依赖和界面、拆分播放核心、实现 Linux MPRIS2 桌面集成**。产物仍是一款功能完整、可靠的本地音乐播放器，并为后续 QuickJS/LX Music 音源支持提供可扩展结构。

### 1.1 P0 必须达成

- 本地 MP3、FLAC、WAV、OGG 播放、暂停、停止、上一首/下一首、Seek、音量和播放模式正常。
- 本地扫描、搜索、只读元数据、封面、LRC 歌词、播放队列正常。
- 保留托盘、键盘快捷键、深浅主题；将 About 并入设置。
- UI、MPRIS 与托盘命令均通过**同一套 Rust 播放命令和状态**。
- Linux 实现 MPRIS2，供 `playerctl`、Waybar、桌面媒体键等客户端控制。
- 量化裁剪前后的内存、CPU、线程、启动耗时与稳定性。

### 1.2 明确不在 P0 实现

- QuickJS、LX Music JS 源兼容层、网络请求引擎、在线音频流。
- 在线音乐搜索、歌单同步、账号系统、下载器。
- MPRIS `TrackList`、`Playlists` 接口及 `OpenUri` 的完整实现。
- 跨平台媒体集成的统一框架、音效/DSP、可视化频谱。
- 借重构机会大规模替换 Rodio、Slint、渲染器或全项目异步运行时。

## 2. 功能处理决策

| 功能/子系统 | P0 决策 | 具体动作 | 验收重点 |
|---|---|---|---|
| 频谱、FFT、采样 tap | **完全删除** | 移除 `src/spectrum.rs`、`ui/spectrum.slint`、相关线程、定时器、属性、依赖 | 不再创建 FFT 线程；播放源不再包裹 `TapSource` |
| 本地音乐标签编辑 | **完全删除** | 移除读取编辑态、封面/歌词上传、标签写入和编辑弹窗 | 无修改音乐文件元数据的入口 |
| 本地音乐文件删除 | **完全删除** | 移除 `DeleteSong`、`std::fs::remove_file` 路径与确认弹窗 | UI 不会删除磁盘文件；未来“从列表移除”须是不同命令 |
| 拼音排序 | **删除** | 去掉 `pinyin`，改为普通字符串比较 | 标题/歌手/时长仍可排序；行为变化明确 |
| 多语言 | **缩减** | 只保留中文、英文资源与切换 | 两种语言构建正常；默认回退有效 |
| About 页面 | **合并** | 不再单独导航，关于信息加入设置页 | 版本、上游、许可证信息仍可查看 |
| 本地扫描及搜索 | **保留** | 保留功能；移至 `library` 模块 | 大曲库无明显性能回退 |
| 读取歌曲元信息 | **保留** | `lofty` 仍负责只读标签、封面/歌词 | 不因删除编辑器破坏展示 |
| LRC 与封面展示 | **保留** | 整理为独立读取/解析能力 | 内嵌及旁路歌词工作正常 |
| 播放历史、模式、队列 | **保留并整理** | 由 Rust 播放核心维护 | 上一首、下一首、循环/随机一致 |
| 系统托盘、快捷键 | **保留** | 事件统一下发到核心 | 与 MPRIS 状态一致 |
| 系统主题 | **保留** | 可后续优化轮询；本次不重写 | 自动跟随及手动切换正常 |
| MPRIS2 | **新增（Linux）** | D-Bus 媒体控制和元数据同步 | `playerctl` 等能发现与操控 |

**注意：**“完全删除编辑能力”不代表删掉 `lofty`、`image` 或 `rfd`。它们仍分别用于只读元数据、封面解码和选择音乐目录。

## 3. 文件级删除与裁剪

### 3.1 频谱链路——整体移除

- 删除：`src/spectrum.rs`、`ui/spectrum.slint`。
- `src/main.rs`：删除 `mod spectrum`、`SpectrumChunk`、`TapSource`、`fft_tx/rx`、`spectrum_enabled`、FFT worker、`build_spectrum_timer`、`PlayerCommand::SetShowSpectrum`；将播放解码后的 `Source` 直接 `player.append(source)`。
- `ui/app.slint`：删除 `SpectrumView` import、频谱背景/组件、`PlaybackGlobal.spectrum`、`show_spectrum` 以及相应 callback。
- `ui/settings_panel.slint`：删除频谱设置项及 callback。
- `src/config.rs`：从新配置结构中去除 `show_spectrum`；为旧配置保留兼容加载策略（见 §6）。
- `Cargo.toml`：移除 `rustfft`。
- 删除/更新相关测试和文档，确保没有失效的 Slint 属性引用。

### 3.2 元数据编辑——只保留读取能力

- `src/main.rs`：删除 `PlayerCommand::LoadSongEditMetadata`、`UpdateSongMetadata` 及处理分支、编辑弹窗回调、封面/歌词文件选择回调、保存成功/失败相关 UI 驱动代码。
- `src/utils.rs`：删除 `SongEditMetadata`、`CoverPreview`/`UploadedCover` 等仅用于编辑的类型、`read_song_edit_metadata`、`load_uploaded_cover`、`read_uploaded_lyrics`（若仅被编辑路径使用，则删除；若 LRC 读取有调用，先提取纯读取逻辑）、`write_song_metadata` 及相关上传/写入测试。
- `ui/song_list.slint`：删除 `SongEditPopup` 与编辑上下文菜单；保留普通歌曲展示及播放事件。
- `ui/app.slint`：删除 `edit_*` 属性、弹窗、`load_song_edit_metadata`/`save_song_metadata` 等 callback。
- **保留/补测**：`read_meta_info`、`read_album_cover`、`read_lyrics`、`parse_lyrics` 和必需的 Slint 封面转换逻辑。

### 3.3 磁盘删除功能——删除所有写磁盘入口

- `src/main.rs`：删除 `PlayerCommand::DeleteSong(PathBuf)` 分支（当前会调用 `std::fs::remove_file`）。
- `ui/song_list.slint`、`ui/app.slint`：删除删除菜单、`DeleteConfirmPopup`、`pending_delete_song`、`delete_song` 等 callback。
- 保留“重扫目录”以刷新歌曲列表，不引入隐式文件删除。
- 若未来需要从自建播放列表中移除歌曲，必须使用单独的 `RemoveFromPlaylist` 语义，禁止映射到 `remove_file`。

### 3.4 拼音排序与其它精简

- `src/utils.rs`：替换 `pinyin::ToPinyin`、`get_chars` 中的拼音排序逻辑，改用确定性的普通字符串键；删除没有其他调用点的拼音辅助函数。
- `Cargo.toml`：删除 `pinyin`。
- 对 `rayon` 做**测量后决策**：扫描、排序、搜索当前使用并行迭代器；先替换局部路径，再对 1k/10k 歌曲库测速，满足性能要求后才移除依赖。
- 将 About 页面从 `ui/app.slint`、`ui/sidebar.slint` 的独立导航移走；相关文本放入 `ui/settings_panel.slint`。
- 精简翻译目录和编译时语言资源配置（`build.rs`），但不移除整个翻译机制。
- **暂不删除**：`single-instance`、`rodio`、`slint`、`lofty`、`image`、`rfd`、`rand`、托盘、歌词、主题。

## 4. 结构拆分

现有 `src/main.rs` 集中实现命令分发、音频设备、播放逻辑、UI 回调、定时器及初始化。P0 应采取**小步迁移**，而不是一次性重写架构。

### 4.1 目标目录

```text
src/
├── main.rs                  # 组装依赖、启动 UI、事件循环
├── app.rs                   # UI callback / 启动协调 / 状态投影
├── player/
│   ├── mod.rs               # PlayerCore / 播放驱动
│   ├── command.rs           # PlayerCommand
│   ├── state.rs             # PlaybackState / PlaybackEvent
│   └── local.rs             # 本地文件音频源
├── library/
│   ├── mod.rs
│   ├── scan.rs              # 本地扫描、筛选、排序
│   ├── metadata.rs          # 只读音乐信息、封面
│   └── lyrics.rs            # LRC / 内嵌歌词
├── integrations/
│   ├── mod.rs
│   └── mpris.rs             # Linux-only MPRIS2 server
├── config.rs
├── logger.rs
└── slint_types.rs

ui/
├── app.slint
├── control_panel.slint
├── song_list.slint
├── search_panel.slint
├── lyrics_panel.slint
├── settings_panel.slint
├── sidebar.slint
├── theme.slint
└── ...                      # 其它仍被引用的 UI 组件
```

### 4.2 内部状态与命令契约

**唯一事实来源：** 播放核心维护 `PlaybackState`；Slint 与 MPRIS 只订阅状态，不各自维护另一套可变播放事实。UI 特有的展开状态、选中页、滚动位置仍留在 UI。

建议 P0 最小命令集合：

```rust
pub enum PlayerCommand {
    Play,
    Pause,
    TogglePlayPause,
    Stop,
    PlayTrack(TrackId),
    Next,
    Previous,
    SeekRelative(std::time::Duration, SeekDirection),
    SeekTo(std::time::Duration),
    SetVolume(f32),
    SetShuffle(bool),
    SetLoopMode(LoopMode),
}

pub enum PlaybackStatus { Playing, Paused, Stopped }
pub enum LoopMode { None, Track, Playlist }
pub enum SeekDirection { Forward, Backward }
```

> 上面是**设计接口示意**，不是可直接替换的完整编译代码。`TrackId` 需定义稳定标识，实际命令还可包含请求来源或结果回执。保留当前 UI 的播放操作时，务必将 UI 的 `SongInfo.id`（列表索引）与稳定歌曲 ID 解耦，避免排序/重扫造成错误定位。

`PlaybackState` 至少包含：状态、当前 TrackId 与只读歌曲元信息、position、duration、volume、shuffle、loop_mode、能否 Seek/Next/Previous。`Track` 先只需覆盖本地来源，但不应永久把 `song_path` 当作全部歌曲的唯一标识；后续在线音乐将需要 `TrackOrigin` 扩展。

### 4.3 通信方向与线程约束

```text
Slint UI ────────────┐
System tray ─────────┼──> PlayerCommand ──> PlayerCore ──> Rodio
MPRIS (Linux D-Bus) ─┘                            │
                                                  ▼
                                           PlaybackEvent / State
                                                  │
                                    ┌─────────────┴─────────────┐
                                    ▼                           ▼
                             Slint UI 投影              MPRIS 属性/信号
```

- Slint GUI 只能在 GUI 线程更新，跨线程使用 `slint::invoke_from_event_loop`。
- D-Bus 方法不能直接操纵 `rodio::Player`、Slint 全局属性或阻塞 GUI 线程；只发送命令至核心。
- `Play`、`Pause`、`TogglePlayPause` 与 `Stop` **必须有独立语义**：上游目前 `PlayerCommand::Pause` 是切换行为，不能直接映射 MPRIS `Pause()`。
- 在播放器状态变更时集中广播事件；不让 MPRIS 通过读取 Slint 属性获取播放事实。
- 不依赖硬编码的 UI 列表索引推进播放；把队列/历史逻辑迁入核心。
- 本地歌曲文件缺失、解码失败时返回明确错误；避免 `expect` 使整个播放器崩溃。
- P0 不需要把整个应用迁移到 Tokio。Linux MPRIS 可拥有独立异步执行环境，具体依赖以选定 crate 的实现为准。

## 5. MPRIS2 桌面集成（P0 新增）

### 5.1 集成与依赖

优先评估 Rust 的 `mpris-server`（服务端）。不能误用主要用于**控制其他播放器**的 `mpris` 客户端 crate。

```toml
[target.'cfg(target_os = "linux")'.dependencies]
mpris-server = "0.9"
futures-channel = "0.3"
futures-lite = "2"
url = "2"
tempfile = "3"
```

实际实现采用 `mpris-server = "0.9"`（与当前工具链及 `zbus 5.19` 兼容），配合 `futures-lite` / `futures-channel` 运行轻量事件循环。实现代码用 `#[cfg(target_os = "linux")]` 隔离在 `src/integrations/mpris.rs`，Windows/macOS 不依赖该模块。

D-Bus session bus 服务名：`org.mpris.MediaPlayer2.zeedlelocal`（`APP_ID = "zeedlelocal"`）；对象路径固定 `/org/mpris/MediaPlayer2`，与 `slint::set_xdg_app_id(crate::APP_ID)` 及 `packager/zeedlelocal.desktop` 保持一致。

### 5.2 必须实现的接口

| 接口 | 方法/属性 | 行为 |
|---|---|---|
| `org.mpris.MediaPlayer2` | `Identity`, `DesktopEntry`, `CanRaise`, `CanQuit`, `HasTrackList` | 标识应用，宣告 `HasTrackList=false` |
| `org.mpris.MediaPlayer2.Player` | `Play`, `Pause`, `PlayPause`, `Stop` | 转发统一播放器命令 |
| 同上 | `Next`, `Previous` | 使用核心队列/历史逻辑 |
| 同上 | `Seek(offset)`, `SetPosition(track_id, position)` | 微秒单位转换；`SetPosition` 校验当前 TrackId |
| 同上 | `PlaybackStatus`, `Position` | 反映真实状态；Position 可通过 getter 回答，不需要高频广播 |
| 同上 | `Metadata` | `mpris:trackid`, `mpris:length`, `xesam:title`, `xesam:artist`, `mpris:artUrl` |
| 同上 | `Volume` | MPRIS 浮点值映射内部音量 |
| 同上 | `Shuffle`, `LoopStatus` | 由内核状态支持，循环值为 `None` / `Track` / `Playlist` |
| 同上 | `CanControl`, `CanPlay`, `CanPause`, `CanSeek`, `CanGoNext`, `CanGoPrevious` | 与实际能力一致，不能全部无条件返回 true |
| 同上 | `Seeked` 信号、属性变化通知 | 完成 Seek、歌曲切换和播放状态变化后正确更新 |

元数据注意事项：

- `mpris:trackid` 是**D-Bus object path**，不是歌曲文件名或任意字符串；当前歌曲应映射到稳定且合法的路径。
- `mpris:length`、`Position`、`Seek`、`SetPosition` 使用**微秒**；Zeedle 当前进度主要使用秒，统一在边界转换。
- `xesam:artist` 是**字符串数组**，即使仅有一位歌手也不能直接填字符串。
- `mpris:artUrl` 使用客户端可访问的 URI；本地封面如来自标签内嵌图片，先生成到应用缓存并导出 `file://` URI，切歌时清理/复用缓存；无封面时省略或使用稳定默认封面。
- `LoopStatus`：原有 `Recursive` 映射 `Track`；现有 `InOrder` 在列表末尾回绕，语义更接近 `Playlist`。如果新增 `None`，需同时设计“列表末尾停止”的行为；`Random` 应作为独立 `Shuffle` 属性，而非继续与循环方式互斥。
- MPRIS 不能调用 `OpenUri`，除非 P0 真正实现了对应 URI 的打开能力；不实现时应明确 `SupportedUriSchemes` 能力。

### 5.3 运行时行为

- **无 D-Bus session bus 时允许启动播放器**：仅禁用 MPRIS，记录警告；本地播放不得受影响。
- MPRIS 任务随应用生命周期启动/停止，避免重复抢占服务名或退出后孤儿任务。
- 主窗口隐藏时，MPRIS 仍可控制播放器；关闭窗口与彻底退出的语义应明确。
- D-Bus 连接/方法错误不能导致主进程 panic。
- 属性变更以真实状态为依据：播放状态、当前曲目、音量、循环/随机改变时通知客户端；不要每 200ms 发送一次全量 `PropertiesChanged`。

### 5.4 Linux 桌面打包集成

- 为应用提供稳定的 `.desktop` 文件，`Name`、`Exec`、`Icon`、`StartupWMClass`（如需）、`XDG app_id` 彼此对应。
- 视最终打包方案考虑 `DBusActivatable`：**没有真正实现 D-Bus 激活时不要宣称支持它**。
- 保留现有系统托盘，但托盘按钮与 MPRIS 走同一命令路径。
- 优先验证 Arch Linux + niri/Wayland，并覆盖常见 X11 或其它 Linux 桌面环境的基本控制行为。

## 6. 配置迁移与兼容

上游 `src/config.rs` 使用 TOML（默认位置 `~/.config/zeedle/config.toml`）。建议 Fork 使用**独立应用名称和配置目录**，避免覆盖上游用户配置。必要时提供显式导入而不是静默迁移。

- 新配置移除 `show_spectrum` 等无效字段；保留 `song_dir`、音量、播放模式、当前曲目与进度等核心字段。
- 旧版配置出现额外字段时优雅忽略；若字段重命名或类型变化，设计默认值与迁移函数，避免反序列化失败后直接丢失全部配置。
- 对旧播放模式与新的 `LoopMode + Shuffle` 映射制定明确迁移规则。
- UI 页面索引由 5 页变为 4 页（曲库、搜索、歌词、设置）时，不能直接重用旧的 `current_page=4`；要有明确回退策略。
- 保留上游配置文件不变，新的配置由 Fork 单独保存。

## 7. 推荐实施顺序与提交划分

### P0.0 — 建立基线

- [x] `cargo test`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo build --release` 通过。

### P0.1 — 纯删除与裁剪

- [x] 删除频谱全链路，移除 `rustfft`（`src/spectrum.rs`、`ui/spectrum.slint`、`TapSource`、`show_spectrum` 等全部清理）。
- [x] 删除元数据编辑与测试中对应写入代码，保留读取测试（`library::metadata::tests`、`library::lyrics::tests`）。
- [x] 删除本地文件删除功能及确认 UI。
- [x] 删除拼音排序，替换排序实现（`library::scan::sort` 普通字符串比较 + `TrackId` 稳定 tie-break），移除 `pinyin`。
- [x] 合并 About 至设置页（`ui/settings_panel.slint`）；仅保留英文（`""`）与简体中文（`"zh_CN"`）。
- [x] 清理无引用资源、依赖、callbacks、配置项与文档（`config::tests` 验证旧版 `play_mode` / `current_page=4` / `lang='fr'` / `show_spectrum` 兼容与回退）。
- [x] 保持本地播放可用（仅保留 `rayon` 用于 `>= 32` 曲目并行扫描，详见 P0.4 实测）。

### P0.2 — 播放核心与库模块拆分

- [x] 提取 `src/player/command.rs` 与 `src/player/state.rs`。
- [x] 将本地文件读取/解码放到 `src/player/local.rs`，其余 Rodio 控制放到 `src/player/mod.rs`。
- [x] 将扫描、排序、只读元数据、歌词解析拆至 `src/library/`（`scan.rs`、`metadata.rs`、`lyrics.rs`）。
- [x] 将 UI 回调注册、UI 状态投影整理至 `src/app.rs`。
- [x] 将队列与历史推进逻辑（`Queue`）从 Slint 状态移至 Rust 播放核心。
- [x] 保证命令、状态变更及错误上报不依赖 UI 线程同步读取。
- [x] 增加播放状态机、队列顺序、曲库排序/搜索与歌词回退单元测试（19 个单元测试全部通过）。

### P0.3 — MPRIS2

- [x] 添加 Linux-only 依赖（`mpris-server = "0.9"` 等）与 `src/integrations/mpris.rs`，注册 `org.mpris.MediaPlayer2.zeedlelocal`。
- [x] 实现基础 Root/Player 接口及 `Play`/`Pause`/`PlayPause`/`Stop`/`Next`/`Previous`。
- [x] 实现 `Seek`、`SetPosition`、稳定 `TrackId`（`/dev/zeedlelocal/track/local_<hex>`）、微秒转换及 `Seeked` 信号。
- [x] 实现标题、歌手数组、时长与封面缓存元数据（`cache_cover` 写入 `$XDG_CACHE_HOME/zeedlelocal/art/*.png`，切歌/退出自动清理）。
- [x] 实现 `Volume`、`Shuffle`、`LoopStatus` 和实际能力属性（`CanControl`、`CanPlay`、`CanPause`、`CanSeek`、`CanGoNext`、`CanGoPrevious`），以及差异化 `PropertiesChanged`（`changes()` 不广播纯进度变化）。
- [x] 确保所有命令经过 `PlayerCommand`（`Controls::execute`），状态由 `PlaybackEvent` / `SharedState` 更新。
- [x] 在 `dbus-run-session` 下完成全链路 D-Bus 集成测试（`mpris_dbus_roundtrip`）及无 D-Bus 降级测试（`mpris_degrades_without_session_bus`）。

### P0.4 — 性能与稳定性验收

- [x] **代码质量与 Linux 构建门禁**：`cargo +nightly fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test`（19 passed, 0 failed, 3 ignored）、`cargo build --release`、`./target/release/zeedle --check-runtime`（~0.01s）、`desktop-file-validate packager/zeedlelocal.desktop` 全部通过（严格遵循仅构建 Linux、不打包策略）。
- [x] **1k / 10k 曲库基准测试（`ZEEDLE_BENCH_DIR` + `cargo test --release measure_library -- --ignored --nocapture`，16 核 Linux，6 轮交替）：**

| 曲库规模 | 方式 | `read` 范围（中位数） | `sort` 范围（中位数） | `search` 范围（中位数） |
|---|---|---|---|---|
| 1,000 首 | `parallel=true` (`rayon`) | 9.14–11.86 ms（9.74 ms） | — | — |
| 1,000 首 | `parallel=false`（串行） | 19.12–25.12 ms（20.61 ms） | 7.52–38.27 µs（14.9 µs） | 33.48–108.14 µs（37.5 µs） |
| 10,000 首 | `parallel=true` (`rayon`) | 90.28–93.54 ms（91.02 ms） | — | — |
| 10,000 首 | `parallel=false`（串行） | 195.73–200.10 ms（197.72 ms） | 217.31–569.92 µs（281.4 µs） | 382.89 µs–1.16 ms（506.6 µs） |

  - **`rayon` 裁剪决策**：在 1k/10k 规模下，并行 `read` 稳定快约 **2.1x–2.2x**（10k 曲库从 ~198ms 降至 ~91ms）；而串行 `sort`（< 0.6ms）与 `search`（< 1.2ms）已足够快。因此保留 `rayon` 仅用于 `scan::scan`（`paths.len() >= 32` 时并行读标签），`sort` 与 `search` 保持串行。
- [x] **依赖与产物体积审计**：
  - `target/release/zeedle`（`lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`）：**27,888,720 字节（~26.6 MiB）**。
  - `cargo tree --depth 1` 确认已彻底移除 `rustfft` 与 `pinyin`，且不包含任何 QuickJS、HTTP 客户端或在线音源依赖。
- [x] **D-Bus 与运行时行为实测**：
  - `dbus-run-session -- cargo test mpris_dbus_roundtrip -- --ignored --nocapture`：完整验证 Root/Player 属性与方法、`TrackId` 校验、微秒 `Seek`/`SetPosition`、`Next`/`Previous`、`Volume`/`Shuffle`/`LoopStatus`、`OpenUri` 拒绝、退出后 D-Bus 服务名释放与封面 PNG 清理；测试进程 `/proc/self/status` 快照 `VmRSS: 26012 kB`、`Threads: 10`。
  - `DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent cargo test mpris_degrades_without_session_bus -- --ignored --nocapture`：无 D-Bus 会话时 `can_run_hidden` 自动设为 `false`，无 panic。
  - 运行时无 FFT 线程与 `TapSource`；`PlayerCore` 空闲阻塞于 `rx.recv()`（零空转），播放态以 200ms 采样；MPRIS 仅在状态变更或 seek 时发送信号。
- [ ] **真机桌面会话人工复核（沙箱外）**：当前沙箱无宿主 Wayland/PipeWire 会话且未安装 `playerctl` CLI；GUI 窗口显隐/托盘、实际音频出声与宿主 `playerctl -p zeedlelocal` 命令需在本地桌面环境运行 `cargo run` / `./target/release/zeedle` 抽检。


## 8. MPRIS 验收命令

```bash
# 查看已注册的播放器
playerctl -l

# 用实际注册的 player 名替换 <player_name>
playerctl -p <player_name> play
playerctl -p <player_name> pause
playerctl -p <player_name> play-pause
playerctl -p <player_name> stop
playerctl -p <player_name> next
playerctl -p <player_name> previous

# 查看状态、元数据及进度
playerctl -p <player_name> status
playerctl -p <player_name> metadata
playerctl -p <player_name> position

# 测试进度跳转（例如向前 10 秒）
playerctl -p <player_name> position 10+
```

额外场景：用户在 GUI 内切歌后 `playerctl metadata` 立刻更新；GUI 暂停后 `status` 为 `Paused`；`playerctl next` 后 GUI 高亮曲目同步；隐藏主窗口不影响媒体键；无 session bus 时应用照常播放。

## 9. 风险与防回归要点

| 风险 | 原因 | 缓解措施 |
|---|---|---|
| 删除编辑器误删歌词读取 | 上游 `read_lyrics` 复用部分读文件逻辑 | 先提取纯 LRC 读取函数，再删除写入路径 |
| 删除频谱影响 Rodio 播放 | 当前音源被 `TapSource` 包裹 | 改为直接 append Decoder；覆盖起播/Seek/切歌 |
| 重构引入播放状态双来源 | 原有状态分散在 Slint 属性中 | 核心状态唯一，UI 和 MPRIS 只是投影 |
| MPRIS 导致 UI 卡顿 | D-Bus 异步任务误跑主线程或持锁回调 | 命令队列解耦；避免阻塞和长时间持锁 |
| 切歌后 SetPosition 作用于旧歌 | MPRIS TrackId 未校验 | 当前 TrackId 精确匹配方可跳转 |
| LoopStatus 语义不一致 | 原有 Random/Recursive/InOrder 互斥 | 明确定义 loop+shuffle 新状态及迁移规则 |
| 配置或应用标识冲突 | Fork 继续使用上游配置目录/单实例名 | Fork 独立配置目录、app ID、MPRIS 名称 |
| 裁剪 Rayon 导致扫描变慢 | 大曲库依赖并行读取/排序 | 有基准后再决定是否彻底删除 |
| 内嵌封面不适合 MPRIS URI | 图片数据目前只在进程内 | 写入可控缓存文件，使用 file URI 暴露 |

## 10. P0 完成定义（Definition of Done）

- [x] 删除：频谱、标签编辑、磁盘歌曲删除、拼音排序；不存在相关运行时逻辑或 UI 入口。
- [x] 保留：本地播放、扫描、搜索、歌词、封面、队列、托盘、快捷键及主题。
- [x] 播放核心、曲库读取和 Linux MPRIS 分模块，`main.rs` 主要负责组装。
- [x] UI、托盘和 MPRIS 使用同一命令/状态源；MPRIS 不直接操纵 UI/音频实例。
- [x] D-Bus 会话端到端测试（`mpris_dbus_roundtrip`）已验证 MPRIS2 发现、控制、元数据、Seek、音量、循环/随机、总线释放与封面清理（真机 `playerctl` CLI 与 GUI 交互留待宿主桌面抽检）。
- [x] 无 D-Bus（`mpris_degrades_without_session_bus`）构建与运行不因 MPRIS 实现而失败；非 Linux 代码边界完整保留在 `#[cfg(target_os = "linux")]` 后。
- [x] 格式化、单元测试、Clippy、Linux Release 构建、`--check-runtime` 与 `packager/zeedlelocal.desktop` 校验全部通过（按仓库仅 Linux 构建策略，不执行打包及跨平台构建）。
- [x] 1k/10k 曲库扫描基准、二进制体积、线程/内存快照与空闲无空转机制已实测并记录在本文件 P0.4。
- [x] P0 不包含任何 QuickJS/LX Music 依赖和在线音源业务代码。

## 11. 参考资料

- Zeedle：<https://github.com/Jordan-Haidee/zeedle>
- Zeedle 入口逻辑：<https://github.com/Jordan-Haidee/zeedle/blob/main/src/main.rs>
- Zeedle 音乐数据逻辑：<https://github.com/Jordan-Haidee/zeedle/blob/main/src/utils.rs>
- Zeedle Slint UI：<https://github.com/Jordan-Haidee/zeedle/tree/main/ui>
- MPRIS 规范：<https://specifications.freedesktop.org/mpris-spec/latest/>
- MPRIS Player 接口：<https://specifications.freedesktop.org/mpris-spec/latest/Player_Interface.html>
- `mpris-server`：<https://docs.rs/mpris-server/latest/mpris_server/>

---

**原则：P0 先完成“少功能、稳架构、强集成”。不要为了兼容未来 LX 脚本而提前引入网络和 JS 运行时；只需要保证播放控制和歌曲信息不被绑定在本地文件路径或 Slint UI 状态上。**
