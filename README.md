# BiMyScribe

将 Bilibili 视频转换为可阅读 Markdown 的本地桌面应用。

BiMyScribe 会自动完成音频下载、FFmpeg 标准化、FunASR 本地转录和文档生成，
并保留模型识别出的说话人信息。媒体文件和转录结果均在本地处理。

## 主要功能

- 支持 Bilibili 视频链接、BV/AV 号和 `b23.tv` 短链接
- 支持多分 P 视频和持久化任务队列
- 通过本地 FunASR 识别中文语音和说话人
- 可在应用内为说话人重命名，无需重新转录
- 生成带时间链接的完整 Markdown 文稿
- 任务中断后可恢复，并支持取消、重试和保留策略
- 可选连接 Ollama 等本地 LLM，对文稿进行可读性整理

## 运行要求

目前支持 macOS。安装前请准备：

- [Rust](https://www.rust-lang.org/tools/install) 1.92 或更高版本
- [FFmpeg](https://ffmpeg.org/)；命令需要位于 `PATH`
- [uv](https://docs.astral.sh/uv/)（默认原生 Runtime 使用）
- [BiMyScribe FunASR Runtime v1.0.0](https://github.com/xiaozhenliu/bimyscribe-funasr-runtime/releases/tag/v1.0.0)

Runtime 项目根目录必须包含 `bimyscribe-runtime.toml`。模型和容器镜像不
包含在本仓库中。Docker Desktop 仅在选择 Docker Runtime 时需要。

## 安装与启动

```bash
git clone https://github.com/xiaozhenliu/bimyscribe.git
cd bimyscribe
cargo run --release
```

首次编译需要下载 Rust 依赖，所需时间取决于网络和设备性能。

另行下载经过验证的原生 Runtime：

```bash
git clone --branch v1.0.0 --depth 1 https://github.com/xiaozhenliu/bimyscribe-funasr-runtime.git
```

## 首次配置

启动后打开“设置”。工作目录与 Markdown 输出目录已经使用 macOS 平台默认值，
也可以通过原生目录选择器修改：

1. **工作目录**：默认位于应用数据目录的 `Jobs`，保存任务中间产物。
2. **Markdown 输出目录**：默认为 `~/Documents/BiMyScribe`。
3. **FunASR Runtime 项目目录**：选择 Runtime v1 项目根目录；保存时会验证
   契约版本和后端。
4. **Runtime 数据目录**：保存 Python 环境、依赖、模型与缓存。通常需要数 GB
   空间，可以直接选择外挂盘。
5. 点击**安装或重新验证 Runtime**。原生 Runtime 会使用冻结的 uv 锁文件；
   Docker Runtime 会验证 Compose 配置和动态挂载。
6. **默认保留策略**：选择任务完成后需要保留的文件。

如果需要 AI 整理，可启用“AI 润色”，并填写本地兼容服务的 Base URL、模型
和 API 格式。当前版本不支持需要身份认证的远程 LLM 服务。

## 使用方法

1. 确认设置中的 Runtime 状态为“已就绪”；Docker Runtime 还需启动 Docker Desktop。
2. 在顶部输入框粘贴 Bilibili 链接或视频编号。
3. 将视频加入队列，等待下载、转码、转录和文档生成完成。
4. 在任务详情中检查识别出的说话人；需要时修改显示名称。
5. 打开最终文稿，或在 Finder 中显示输出目录。

修改说话人名称后，BiMyScribe 会直接重建 Markdown，不会重复转录音频。

## 输出文件

每个任务可能生成：

- `transcript.raw.json`：包含时间戳和说话人编号的结构化转录结果
- `transcript.raw.md`：按原始识别段落生成的 Markdown
- `transcript.readable.md`：整理后的可读正文
- `full.md`：包含视频信息、说话人和可点击时间链接的最终文稿

具体保留哪些中间文件由任务的保留策略决定。

## 当前限制

- 暂无预构建的 `.app` 安装包，需要通过 Cargo 启动。
- 仅支持匿名访问，不支持需要登录或 Cookie 的视频。
- 截图提取功能尚未开放。
- FunASR Runtime 和 FFmpeg 需要单独安装；Docker 只用于可选后端。

## 许可证

[MIT License](LICENSE)

<details>
<summary>English</summary>

## About

BiMyScribe is a local macOS desktop application that turns Bilibili videos into
readable Markdown. It downloads the audio, normalizes it with FFmpeg,
transcribes it through a local FunASR runtime, and preserves detected
speaker information.

### Features

- Bilibili URLs, BV/AV IDs, `b23.tv` short links, and multi-part videos
- Persistent task queue with cancellation, retry, and recovery
- Local FunASR transcription and speaker recognition
- Speaker renaming without re-running transcription
- Markdown output with clickable Bilibili timestamp links
- Optional text refinement through a local LLM service such as Ollama

### Requirements

- macOS
- Rust 1.92 or newer
- FFmpeg available on `PATH`
- uv for the default native runtime
- [BiMyScribe FunASR Runtime v1.0.0](https://github.com/xiaozhenliu/bimyscribe-funasr-runtime/releases/tag/v1.0.0)

FunASR models are not bundled. Docker Desktop is required only for the optional
Docker runtime.

### Install and run

```bash
git clone https://github.com/xiaozhenliu/bimyscribe.git
cd bimyscribe
cargo run --release
```

Download the verified native runtime separately:

```bash
git clone --branch v1.0.0 --depth 1 https://github.com/xiaozhenliu/bimyscribe-funasr-runtime.git
```

### First-time setup

Open Settings and select a compatible FunASR Runtime project. Choose a Runtime
Data directory with several gigabytes of free space; it may be on an external
drive. Install or validate the runtime from Settings. Job data defaults
to the macOS Application Support directory, while Markdown defaults to
`~/Documents/BiMyScribe`; both locations can be changed with the native folder
picker. Choose a retention policy for intermediate files.
Local LLM refinement is optional; authenticated remote LLM services are not
supported in this release.

### Basic workflow

1. Make sure Settings reports that the runtime is ready. Start Docker Desktop
   only when using the Docker backend.
2. Paste a Bilibili link or video ID and add it to the queue.
3. Wait for download, conversion, transcription, and document generation.
4. Review or rename detected speakers in the task details.
5. Open the generated `full.md` document or reveal it in Finder.

### Output

Depending on the retention policy, a task may produce structured transcription
JSON, raw Markdown, refined Markdown, and a final `full.md` document.

### Current limitations

- No prebuilt `.app` bundle is available yet.
- Videos requiring login or cookies are not supported.
- Screenshot extraction is not available yet.
- The FunASR runtime and FFmpeg must be installed separately; Docker is optional.

### License

[MIT License](LICENSE)

</details>
