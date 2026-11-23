# LX 自定义源 P0 验证记录

**选型结论**　Linux 首版采用子进程中的 QuickJS-NG 0.16.2（`rquickjs` 0.14.0）执行脚本。2026-11-23，原版墨澜 2.3.8 在 LX Desktop 2.12.6 和本原型中声明了相同的能力，并为网易云同一首歌返回了可解码的地址。这满足 P1 开始取址接入的条件。其余四个平台目前有本地协议样本，真实服务覆盖见下表。

## 在 Linux 复现

在仓库根目录执行。原型是独立 Cargo 包。离线协议测试只连接回环地址，首次编译需要 Cargo 依赖和 C 编译器，测试需要 Python 3。

```sh
cargo test --locked --manifest-path tools/lx-probe/Cargo.toml
```

与 LX 对照时，使用发行包内实际的 `dist/user-api-preload.js` 和 `user-api.html`。测试驱动在 Xvfb 和临时 Electron 配置目录中启动隐藏窗口，通过 LX 的 IPC 事件送入脚本原文和请求。脚本、请求数组、回环端口和断言在两端相同。这里测量的是源宿主，范围到 `musicUrl` 返回。

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

单独探测一个源时，第二个参数为 LX 动作数组，可以用空数组只验证初始化。每行输出含编号，0 为归一化后的能力，后续编号与输入动作顺序一致。默认只输出结果 URL 的 origin。供解码器读取完整结果时，可在脚本路径前传入 `--full-url`。

```sh
jq '[.[] | select(.source == "wy")]' tools/lx-probe/fixtures/tracks.json |
  tools/lx-probe/target/debug/lx-probe tools/lx-probe/fixtures/minimal.js /dev/stdin
```

## 样本与结果

基线为 SealMusic `73cfd57`、LX Music Desktop `v2.12.6`（Arch 包 `2.12.6-1`，Electron `42.11.18`）和源 API `2.0.0`。LX 发布包 `app.asar` 的 SHA-256 为 `2931ff09091602a98e8c065bc39e9bb24e791d3d3fcbdcddcc880eed27d58944`。驱动直接使用该包的事件分发、能力过滤、HTTP 库和 Electron 跨上下文数据转换。[上游 preload](https://github.com/lyswhut/lx-music-desktop/blob/v2.12.6/src/main/modules/userApi/renderer/preload.js) · [自定义源协议](https://lxmusic.toside.cn/desktop/custom-source)

[固定输入](fixtures/tracks.json) 是传给脚本的 `{source, action, info: {type, musicInfo}}`。本地测试只在 `musicInfo` 中追加临时回环服务地址。音质数组 `types`、映射 `_types` 和平台字段按脚本入参保存。[上游曲目调用](https://github.com/lyswhut/lx-music-desktop/blob/v2.12.6/src/renderer/core/music/utils.ts)

| 平台 | 固定输入中核对的标识 | HTTP 样本 1.0，双端 musicUrl / 128k | 墨澜 2.3.8 双端初始化返回的 qualitys | 墨澜真实取址 / 解码 |
|---|---|---|---|---|
| kw | 字符串 songmid | 通过 | 128k、320k、flac、flac24bit | 未测 |
| kg | 数字 songmid、曲目 hash、分音质 hash | 通过 | 128k、320k、flac | 未测 |
| tx | 字符串 songmid、strMediaMid | 通过 | 128k、320k、flac、flac24bit | 未测 |
| wy | 数字 songmid 29947420 | 通过 | 128k、320k、flac | 128k 双端通过 |
| mg | 字符串 songmid、copyrightId | 通过 | 128k、320k、flac、flac24bit | 未测 |

- [最小源 1.0](fixtures/minimal.js) 注册异步 handler 并返回固定 HTTPS 地址，用作宿主异常后的重新启动检查。
- [HTTP 与 buffer 源 1.0](fixtures/http.js) 的 SHA-256 为 `6510b9d21bf30f4cf6fcbc960f9c14d1d494e0a6e07e4c8188a1ec84ffabc665`。两端都初始化成功，五个动作都返回同一回环服务的 `/tone.wav`。测试从返回地址读取单声道 8 kHz、16 位 PCM WAV，解码后逐字节核对 800 帧样本。
- 墨澜 2.3.8 的 SHA-256 为 `a5cec42157c4f6bb86c51ec56c905f5c86bac8ac492c1a9e944a1c9725c279cc`。实测路径读取 `lx.currentScriptInfo`，调用 `lx.on`、`lx.send`、`setTimeout`、`lx.request`、`lx.utils.buffer.from` 和 `lx.utils.buffer.bufToString`。对 Alan Walker《Fade》的 `wy/musicUrl/128k` 请求，两端均返回网易云 CDN 的媒体地址。两轮实测分别命中 `m801.music.126.net` 和 `m701.music.126.net`。分别读取前 65,536 字节，FFmpeg 各解码出 1 秒、176,400 字节的 44.1 kHz 双声道 16 位 PCM，ffplay 各播放 1 秒到 SDL 虚拟音频设备。物理扬声器与 SealMusic 播放栏属于后续桌面验收。
- 同一脚本的外部失败样本为 `wy/musicUrl/128k`、数字 songmid 186016。两端均成功初始化，后端返回曲目级 code 404 和空 URL，取址失败，未进入解码。

## 回调与失败语义

| 用例 | LX 2.12.6 预期与观测 | QuickJS 原型实测 |
|---|---|---|
| 初始化、能力、动作 | 定时器触发 inited，过滤未知动作和音质，保留 qualitys 拼写；request handler 返回 Promise | 相同；派发前还会拒绝未声明的平台、动作或音质 |
| 请求体 | GET；POST 字符串、自定义头、JSON 对象、扁平 form | 服务端核对请求体、标头及 MIME 类型，通过 |
| 回调成功 | err 为 null，201 Created，响应头键为小写。JSON 同时进入回调第三参数与 resp.body，非 JSON 文本保留为字符串 | 同一输入、相同字段值；即使 JSON 响应使用 text/plain 也会解析 |
| 原始数据 | resp.raw 在脚本侧为 Uint8Array；resp.bytes 为字节数。二进制向量转 hex 得 0080ff，UTF-8 的“音”转 base64 得 6Z+z | 相同 |
| 拒绝与重复初始化 | 动作 Promise 拒绝传播 fixture rejection，第二次 inited 拒绝 Script is inited | 相同 |
| 取消慢请求、HTTP 超时 | 两次均回调 Error、null、null，本机 Needle 错误文案均为 socket hang up | 相同回调形状；取消报告 request cancelled，超时报告 HTTP timeout |
| 宿主边界 | 原型独立验收 | 过大响应与非 HTTP(S) 取址失败；死循环被 VM 中断；悬空 Promise 达到父进程时限后退出；新宿主可成功初始化并取址 |

[失败样本 1.0](fixtures/failures.js) 和[回环服务与断言](tests/protocol.py)保存了逐项复现输入。自动离线测试与真实服务测试分别运行。

## 首版宿主子集

父进程按需启动同一探针的 worker，通过有编号、最大 4 MiB 的 JSON 行交换初始化、动作、HTTP/定时器回调、取消、结果和错误。父进程独占 HTTP(S)，按实际读取的字节数限制响应为 512 KiB，最多同时保留 32 项回调。脚本与请求文件各限 1 MiB。VM 堆限额 48 MiB、栈限额 1 MiB，每次执行限时 2 秒。每次初始化与动作另外受父进程 12 秒墙钟时限约束，包含 IPC 等待；完成或失败后回收子进程。JS 侧只绑定显式的宿主函数，模块加载器、文件和系统命令接口均未启用。

P1 可复用已测的五平台能力声明、`musicUrl`、Promise、`setTimeout/clearTimeout`、`lx.request` 和 buffer 的 UTF-8、base64、hex 子集。HTTP 原型直连、禁用重定向，`timeout` 约束整次 HTTP 传输；上游使用 Needle 的响应超时。非标准 HTTP 原因短语、嵌套表单、容错编码与压缩传输尚无等价性结论。

留待后续样本决定的接口包括 `lx.request(..., {formData})`、`lx.utils.crypto.aesEncrypt/rsaEncrypt/randomBytes/md5`、`lx.utils.zlib.inflate/deflate`、`lx.send(updateAlert)`、`window.lx` 和 `local.lyric/pic/musicUrl`。本次实测覆盖的在线音质为 128k；其他音质只采集了声明。

## Linux 回归结果

- 探针包通过 nightly rustfmt、全目标 Clippy（警告视为错误）和 `cargo test --locked`。指定 LX 发行包及墨澜脚本后，协议、拒绝路径、异常后重启、音质过滤和联网解码五组检查全部通过。
- 主程序通过 nightly rustfmt、全目标 Clippy 和 `cargo test --locked`（24 项通过，3 项标记为 ignored）。以 `dbus-run-session` 单独运行的 `mpris_dbus_roundtrip` 通过。`cargo build --release --locked` 和 `./target/release/sealmusic --check-runtime` 均成功。
