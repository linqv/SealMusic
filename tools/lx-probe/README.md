# LX 自定义源兼容与验证记录（P0 / P2）

**选型与阶段结论**　Linux 端采用子进程中的 QuickJS-NG 0.16.2（`rquickjs` 0.14.0）执行脚本。P0 阶段原版墨澜 2.3.8 在 LX Music Desktop 2.12.6 和本原型中声明了相同的能力，并为网易云同一首歌返回了可解码地址；P1 已将在线单曲播放接入 SealMusic（`ed7a43a`）；P2 已补齐 `utils.buffer` / `utils.crypto` / `utils.zlib`、`lx.request` 表单与线级响应、`updateAlert`、`window.lx` 以及 `local` 源歌词/封面按需增强与显式取址，并通过 LX 2.12.6 双端协议对照与桌面端回归。

## 在 Linux 复现

在仓库根目录执行。探针包是独立 Cargo 包，离线协议测试只连接回环地址，测试需要 Python 3。

```sh
cargo test --locked --manifest-path tools/lx-probe/Cargo.toml
```

与 LX Music Desktop 2.12.6 对照时，使用发行包内实际的 `dist/user-api-preload.js` 和 `user-api.html`。测试驱动在原生 Wayland（`--ozone-platform=wayland`）和临时 Electron 配置目录中启动隐藏窗口，通过 LX 的 IPC 事件送入脚本原文和请求。脚本、请求数组、回环端口和断言在两端相同。

```sh
cargo build --locked --manifest-path tools/lx-probe/Cargo.toml
python3 tools/lx-probe/tests/protocol.py \
  --electron electron42 --lx-archive /usr/lib/lx-music-desktop/app.asar
```

复测第三方服务需要本地脚本、网络、FFmpeg 与 ffplay。驱动校验脚本摘要，只在内存中使用完整媒体 URL。服务可用性随时间变化。

```sh
python3 tools/lx-probe/tests/protocol.py \
  --lx-archive /usr/lib/lx-music-desktop/app.asar \
  --molan-script 'test-lxjs/墨澜音乐源v2.3.8.js'
```

复现 P2 本地源桌面端集成验收（需在 Wayland 会话中运行，并具备 `dbus-run-session` 与 `busctl`，可选 `--captures <dir>` 使用 `grim` 保存截屏；驱动内部会自动清除 `DISPLAY` 与代理环境变量以确保原生 Wayland 与回环直连）：

```sh
cargo build --release
dbus-run-session -- python3 tools/lx-probe/tests/local_desktop.py \
  --binary target/release/sealmusic
```

单独探测一个源时，第二个参数为 LX 动作数组，可以用空数组只验证初始化。每行输出含编号，0 为归一化后的能力，后续编号与输入动作顺序一致。默认只输出结果 URL 的 origin。供解码器读取完整结果时，可在脚本路径前传入 `--full-url`。

```sh
jq '[.[] | select(.source == "wy")]' tools/lx-probe/fixtures/tracks.json |
  tools/lx-probe/target/debug/lx-probe tools/lx-probe/fixtures/minimal.js /dev/stdin
```

## 样本与基线

基线为 SealMusic `73cfd57`（P1 交付 `ed7a43a`）、LX Music Desktop `v2.12.6`（Arch 包 `2.12.6-1`，Electron `42.11.18`）和源 API `2.0.0`。LX 发布包 `app.asar` 的 SHA-256 为 `2931ff09091602a98e8c065bc39e9bb24e791d3d3fcbdcddcc880eed27d58944`。驱动直接使用该包的事件分发、能力过滤、HTTP 库和 Electron 跨上下文数据转换。[上游 preload](https://github.com/lyswhut/lx-music-desktop/blob/v2.12.6/src/main/modules/userApi/renderer/preload.js) · [自定义源协议](https://lxmusic.toside.cn/desktop/custom-source)

[固定输入](fixtures/tracks.json) 是传给脚本的 `{source, action, info: {type, musicInfo}}`。本地测试只在 `musicInfo` 中追加临时回环服务地址。音质数组 `types`、映射 `_types` 和平台字段按脚本入参保存。[上游曲目调用](https://github.com/lyswhut/lx-music-desktop/blob/v2.12.6/src/renderer/core/music/utils.ts)

主要离线样本与摘要：

- [最小源 1.0](fixtures/minimal.js)（SHA-256 `a4aa1fc015000eb56b76134ecd744cbcb9d463c2af5cb006500a30616fd4e194`）：注册异步 handler 并返回固定 HTTPS 地址，用作宿主异常重启与未声明音质校验。
- [HTTP 与 buffer 源 1.0](fixtures/http.js)（SHA-256 `6510b9d21bf30f4cf6fcbc960f9c14d1d494e0a6e07e4c8188a1ec84ffabc665`）：覆盖五平台 `musicUrl/128k`、POST/GET/JSON/form、`resp.raw`、`resp.bytes` 与回环 WAV 逐帧校验。
- [P2 本地源与向量样本 1.0](fixtures/local.js)（SHA-256 `0b42ed37dba270307cf37ee9bc77f2f2c9e247c0ad49143246db58b8e6987127`）：通过 `window.lx` 覆盖全部 buffer 编码、AES/RSA/MD5/randomBytes、zlib 编解码、错误路径、非标准 HTTP 原因短语、gzip 线级字节、嵌套 `formData`、`updateAlert` 单次限制以及 `local` 的 `musicUrl`/`lyric`/`pic`。
- [失败样本 1.0](fixtures/failures.js)（SHA-256 `a46302f0a1be769d5d5ca67b2c2b449dcd56a05bb32a6da415c464a31c4a265c`）：覆盖拒绝 Promise、重复 `inited`、非法 URL、取消、超时、过大响应、死循环与悬空 Promise。
- 墨澜 2.3.8（SHA-256 `a5cec42157c4f6bb86c51ec56c905f5c86bac8ac492c1a9e944a1c9725c279cc`，外部脚本）：实测路径覆盖 `wy/musicUrl/128k` 真实取址与解码。

## 兼容清单（初始化 / 取址 / 解码 / 歌词 / 封面）

下表区分各平台与动作的成功与失败路径及复现输入；`双端通过` 表示 QuickJS 宿主与 LX Music Desktop 2.12.6 发行包在相同输入下结果一致。

| 平台 / 范围 | 标识字段与复现输入 | 初始化 | 取址（`musicUrl`） | 解码 / 播放 | 歌词（`lyric`） | 封面（`pic`） |
|---|---|---|---|---|---|---|
| `kw` | 字符串 `songmid`；[tracks.json](fixtures/tracks.json) + [http.js](fixtures/http.js) / 墨澜 2.3.8 | 双端通过（墨澜声明 `128k/320k/flac/flac24bit`） | 回环 `128k` 双端通过；墨澜真实取址未测 | 回环 WAV 800 帧逐字节通过 | 不适用（上游协议未对在线平台开放） | 不适用 |
| `kg` | 数字 `songmid`、曲目 `hash`、分音质 `hash`；[tracks.json](fixtures/tracks.json) + [http.js](fixtures/http.js) / 墨澜 2.3.8 | 双端通过（墨澜声明 `128k/320k/flac`） | 回环 `128k` 双端通过；墨澜真实取址未测 | 回环 WAV 800 帧逐字节通过 | 不适用 | 不适用 |
| `tx` | 字符串 `songmid`、`strMediaMid`；[tracks.json](fixtures/tracks.json) + [http.js](fixtures/http.js) / 墨澜 2.3.8 | 双端通过（墨澜声明 `128k/320k/flac/flac24bit`） | 回环 `128k` 双端通过；墨澜真实取址未测 | 回环 WAV 800 帧逐字节通过 | 不适用 | 不适用 |
| `wy` | 数字 `songmid`（成功 `29947420`，失败 `186016`）；[tracks.json](fixtures/tracks.json) + [http.js](fixtures/http.js) / 墨澜 2.3.8 | 双端通过（墨澜声明 `128k/320k/flac`） | 回环 `128k` 双端通过；墨澜 `29947420` 双端通过，失败样本 `186016` 双端均返回 404 失败 | 回环 WAV 通过；墨澜取址前 65,536 字节经 FFmpeg 解出 1s 44.1kHz PCM、ffplay 虚拟设备播放通过；过期 URL 单次重取与坏音频报错见 [media.rs](../../src/online/media.rs) | 不适用 | 不适用 |
| `mg` | 字符串 `songmid`、`copyrightId`；[tracks.json](fixtures/tracks.json) + [http.js](fixtures/http.js) / 墨澜 2.3.8 | 双端通过（墨澜声明 `128k/320k/flac/flac24bit`） | 回环 `128k` 双端通过；墨澜真实取址未测 | 回环 WAV 800 帧逐字节通过 | 不适用 | 不适用 |
| `local` | `filePath`、`songmid`（同路径）、`ext`、`albumName`；[local.js](fixtures/local.js) + [protocol.py](tests/protocol.py) + [local_desktop.py](tests/local_desktop.py) | 双端通过（`qualitys: []`，仅保留 `musicUrl/lyric/pic`；P1 旧配置无 `local_actions` 时重新启用可导入） | `type: null` 协议双端通过；桌面端右键“使用 LX 音源播放”显式取址成功（默认双击走本地文件直解） | 桌面端显式 `local.musicUrl` 下载并解码回环 WAV 播放通过；未声明 `local.musicUrl` 时返回 `NoQuality` 并安全停止空 sink | 协议双端通过（含 `lyric/tlyric/rlyric/lxlyric`）；桌面端曲目 A 加载并投射至 UI 与 MPRIS，曲目 B 抛错时继续播放 | 协议双端通过；桌面端曲目 A 下载缩放 PNG 并同步至 UI 与 MPRIS 缓存，曲目 B 返回 >4 MiB 响应时丢弃并继续播放 |
| 失败与边界 | [failures.js](fixtures/failures.js)、[protocol.py](tests/protocol.py)（`test_failures_and_restart`、`test_local_result_limits`、`test_undeclared_quality`、`test_edge_cases`）、[local_desktop.py](tests/local_desktop.py) | 重复 `inited` 双端拒绝 `Script is inited`；非数组能力拒绝；超 1 MiB 截断 UTF-8 脚本拒绝 | Promise 拒绝、非 HTTP(S) URL、未声明音质、HTTP 取消/超时/超 512 KiB、死循环、悬空 Promise 均失败并可重启新宿主 | 坏音频报 `Decode`、超 256 MiB 报 `Transfer`；桌面端 `SIGKILL` 杀掉详情 worker 不影响当前本地曲目继续播放 | 主歌词 `> 51,200` 字符双端失败；可选字段达到或超过上限双端置为 `null` | 非 HTTP(S) `file://` 地址双端拒绝；桌面端切歌 A→B 同步清理旧封面缓存、UI 图像与 MPRIS `mpris:artUrl`/`syncedLyrics` |

## API 覆盖表与 Electron 环境差异

| 接口 / 能力 | 已验证行为与复现用例 | 与 Electron / LX 2.12.6 环境差异 |
|---|---|---|
| 全局入口（`globalThis.lx`、`window.lx`） | [local.js](fixtures/local.js) 通过 `window.lx` 解构访问 `utils`，暴露 `EVENT_NAMES`、`env = 'desktop'`、`version = '2.0.0'`、`currentScriptInfo`、`setTimeout`/`clearTimeout` 与空实现 `console` | 仅将 `globalThis.window` 指向 `globalThis`，不提供浏览器 DOM、Electron Renderer 或 Node 全局模块（无 `require`/`process`/`Buffer`） |
| `lx.utils.buffer`（`from`、`bufToString`） | [local.js](fixtures/local.js) 与 `test_edge_cases` 双端验证 UTF-8、`latin1`/`binary`、`ascii`、`utf16le`/`ucs2`、容错 `base64`/`base64url`（忽略非字母表字符、无填充、换行）、部分前缀 `hex`、`ArrayBuffer`（含偏移与长度）及 `{type: 'Buffer', data}` | 返回标准 `Uint8Array` 而非 Node `Buffer` 实例（无 `Buffer.prototype` 方法）；`hex` 解码复用 Rust `from_str_radix`（前导 `+` 号边界略异于 Node），`buffer.from({})`/`true` 落入 `new Uint8Array` 而非抛出 `TypeError` |
| `lx.utils.crypto`（`aesEncrypt`、`rsaEncrypt`、`randomBytes`、`md5`） | [local.js](fixtures/local.js) 双端验证 `md5`、`aes-128-cbc`、`aes-128-ecb`、1024 位 raw RSA（`Padding::NONE` 左侧补零）、`randomBytes(17)`（类型为 `Uint8Array`）与 `randomBytes(0)`，以及非法 IV、非法公钥、负数长度报错 | 通过 OpenSSL 原生绑定实现；`aesEncrypt` 开放 `aes-{128,192,256}-{cbc,ecb}` 6 种模式（Electron/Node 支持全部 OpenSSL cipher 名称）；`rsaEncrypt` 与上游一致固定 128 字节块（仅支持 1024 位公钥）；所有原生工具输入及随机字节输出统一限 4 MiB |
| `lx.utils.zlib`（`inflate`、`deflate`） | [local.js](fixtures/local.js) 双端验证 `deflate`（输出 `789c...` 与 Node 字节一致）、`inflate` 往返以及非法 zlib 流拒绝 | 使用 `flate2` 的 C `zlib` 后端并包装为异步 Promise；输入与解压输出均受 4 MiB 上限约束以防解压炸弹 |
| `lx.request(url, options, callback)` | [http.js](fixtures/http.js)、[local.js](fixtures/local.js) 与 `test_edge_cases` 双端验证 GET/POST、自定义/数字头、JSON（含空白与参数）、扁平 `form`、嵌套 `formData`（自动过滤空嵌套对象/数组）、自定义 HTTP 原因短语 `statusMessage`、gzip 线级 `resp.raw`/`resp.bytes`、201 状态码、响应头小写化、JSON 与文本 body 解析、省略或 `undefined` `options` 及取消/超时回调 | 宿主由 Rust `reqwest` 直连、禁用重定向；`timeout` 约束整次传输（上限 60 秒）而非 Needle 响应超时；响应体上限 512 KiB，单会话最多 32 个并发回调；与 LX 2.12.6 preload 一致均不启用 `multipart`；暂不支持 `Uint8Array` 直接作为非 JSON `body` |
| `lx.send('updateAlert', info)` | [local.js](fixtures/local.js) 双端验证首次发送成功、同一次脚本运行内二次调用拒绝 `The update alert can only be called once.`；[local_desktop.py](tests/local_desktop.py) 验证桌面端弹出更新窗口、不自动打开链接、由用户点击决定 | SealMusic 为短生命周期子进程模型：`PlayerCore` 按脚本启用会话维护 `shown_alert` 去重，导入完成后的异步更新检查在后台至多等待 5 秒不阻塞界面；Rust 侧过滤非 HTTP(S) `updateUrl` 且日志不记录完整更新 URL |
| 宿主隔离与资源边界 | [failures.js](fixtures/failures.js) 与 `test_edge_cases` 验证 | 子进程 IPC 帧上限 4 MiB、脚本与请求文件各限 1 MiB；VM 堆限 48 MiB、栈限 1 MiB、单步执行限 2 秒；单阶段父进程墙钟限 12 秒（导入后后台排空限 5 秒）；无文件模块加载与系统命令接口；异常退出后可重新启动新宿主 |

## 本地歌词字段接收与显示能力

针对 `local` 源 `lyric` 动作返回对象 `{lyric, tlyric, rlyric, lxlyric}`：

| 字段 | 含义 | 协议层接收与校验（`bridge.js` / `lx-probe`） | SealMusic 显示与投影能力（`src/online/local.rs` / UI / MPRIS） |
|---|---|---|---|
| `lyric` | 主歌词（LRC） | 必选字符串，长度 `<= 51,200` 字符；缺失或超长则动作失败 | 经 `parse_lyrics` 解析后显示在歌词页，并编码到 MPRIS `syncedLyrics` 的 `text` 字段 |
| `tlyric` | 翻译歌词 | 可选字符串，长度 `< 5,120` 字符（与 LX 2.12.6 行为一致；非字符串或 `>= 5,120` 置为 `null`） | 与 `lyric` 合并送入 `parse_lyrics`；UI 按时间戳顺序逐行显示（同时间戳行当前仅高亮最后一行），MPRIS 合并为 `translation` |
| `rlyric` | 罗马音歌词 | 可选字符串，长度 `< 5,120` 字符（非字符串或 `>= 5,120` 置为 `null`） | 与 `lyric`/`tlyric` 合并送入 `parse_lyrics`；UI 按时间戳逐行显示，MPRIS 以 `" / "` 拼接至 `translation` |
| `lxlyric` | 逐字歌词 | 可选字符串，长度 `< 8,192` 字符（非字符串或 `>= 8,192` 置为 `null`）；双端对照已验证完整接收 | 当前不解析 `<start,dur>` 逐字时间轴，播放端忽略该字段，不单独渲染逐字高亮 |

封面增强限制：`local.pic` 仅接受 HTTP(S) URL，下载硬限 4 MiB（同时校验 `Content-Length` 与实际流式字节数），解码前设置 `image::Limits`（最大分配 64 MiB、最大宽高 8192px），复用既有 `resize_cover` 缩放到不超过 512px，并写入 MPRIS 临时封面缓存；切歌、停止或禁用脚本时按请求编号与 `TrackId` 丢弃迟到结果并同步清理旧封面与旧歌词，本地音频文件与标签全程保持只读。

## Linux 回归结果

- **代码格式与静态检查**：主仓库与 `tools/lx-probe` 均通过 `cargo +nightly fmt --check` 和 `cargo clippy --all-targets --all-features -- -D warnings`。
- **主程序单元测试**：`cargo test` 通过（49 passed, 0 failed, 3 ignored），含本地详情双编号防覆盖、导入后台排空更新提示、以及空 sink 下 `PlayLocalSource` 失败停播状态机测试。
- **MPRIS D-Bus 集成测试**：`dbus-run-session -- cargo test mpris_dbus_roundtrip -- --ignored` 通过。
- **探针离线与 LX 2.12.6 双端对照**：
  - `cargo test --locked --manifest-path tools/lx-probe/Cargo.toml` 通过（6 passed, 2 skipped）。
  - `python3 tools/lx-probe/tests/protocol.py --electron electron42 --lx-archive /usr/lib/lx-music-desktop/app.asar` 全部离线与双端用例通过（7 passed, 1 skipped，覆盖 `test_roundtrip`、`test_local_vectors`、`test_local_result_limits`、`test_lx_failures`、`test_failures_and_restart`、`test_undeclared_quality`、`test_edge_cases`）。
- **发布构建与桌面端回归**：
  - `cargo build --release` 与 `./target/release/sealmusic --check-runtime` 均成功。
  - `dbus-run-session -- python3 tools/lx-probe/tests/local_desktop.py --binary target/release/sealmusic`（原生 Wayland 会话下通过 MPRIS 驱动 A→B→C 切歌、封面/歌词同步与清理、>4 MiB 封面与 worker `SIGKILL` 崩溃隔离；其余交互路径由 Rust 单元测试覆盖）。
