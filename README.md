# bi2read

将 Bilibili 视频转换为可阅读 Markdown 的本地桌面应用。

bi2read 会自动完成音频下载、FFmpeg 标准化、FunASR 本地转录和文档生成，
并保留模型识别出的说话人信息。媒体文件和转录结果均在本地处理。

## 主要功能

- 支持 Bilibili 视频链接、BV/AV 号和 `b23.tv` 短链接
- 支持多分 P 视频和持久化任务队列
- 通过本地 FunASR 识别中文语音和说话人
- 可在应用内为说话人重命名，无需重新转录
- 生成带时间链接的完整 Markdown 文稿
- 任务中断后可恢复，并支持取消、重试和保留策略
- 可选连接 Ollama 等本地 LLM，对文稿进行可读性整理
- v0.6.0 支持智能文字处理与精选：AI 连接测试、模型更换提示与重生成、短/标准/长三档摘要、单视频文字与说话人搜索、版本化阅读时间估算

## 运行要求

目前支持 macOS。安装前请准备：

- [Rust](https://www.rust-lang.org/tools/install) 1.92 或更高版本；仓库用
  `rust-toolchain.toml` 固定贡献者工具链为 1.95，`Cargo.toml` 的 1.92 是 MSRV
- [FFmpeg](https://ffmpeg.org/)；终端运行时应位于 `PATH`，Finder 启动的 App
  还会检查 `~/.local/bin`、Homebrew 和 MacPorts 的标准目录
- [uv](https://docs.astral.sh/uv/)（通过 Cargo 运行原生 Runtime 时需要；构建 App
  的脚本会自动下载）
- bi2read FunASR Runtime contract v2（contract v1 仍可读取，但不能创建新任务）

Runtime 项目根目录应包含 `bi2read-runtime.toml`；旧项目中的
`bimyscribe-runtime.toml` 仍可读取。模型和容器镜像不包含在本仓库中。
Docker Desktop 仅在选择 Docker Runtime 时需要。

## 安装与启动

App 版本以 [`Cargo.toml`](Cargo.toml) 为唯一来源；当前版本的完整中英文变化、兼容性与
限制见 [`docs/releases/v0.6.1.md`](docs/releases/v0.6.1.md)。

```bash
git clone https://github.com/xiaozhenliu/bi2read.git
cd bi2read
cargo run --release
```

首次编译需要下载 Rust 依赖，所需时间取决于网络和设备性能。

另行下载经过验证的原生 Runtime：

```bash
git clone --branch v2.0.0 --depth 1 https://github.com/xiaozhenliu/bi2read-funasr-runtime.git
```

### 在自己的 Mac 上构建 App

只需要 Rust、Xcode Command Line Tools、Git 和网络连接。脚本会自动下载固定版本
的 Runtime 与 uv，构建 `bi2read.app`，并在签名前询问签名身份：直接回车会
使用免费的 ad-hoc 签名，适合在当前 Mac 自用，不需要 Apple Developer 会员。

```bash
git clone https://github.com/xiaozhenliu/bi2read.git
cd bi2read
scripts/build-macos-local.sh
```

默认产物位于 `target/macos-local/output/bi2read.app`。空间不足时可以把全部
构建数据放到其他磁盘：

```bash
BI2READ_BUILD_ROOT=/absolute/path/to/build scripts/build-macos-local.sh
```

如需使用自己的 Developer ID，在运行前设置
`BI2READ_SIGN_IDENTITY='Developer ID Application: …'`。Developer ID 签名
之后仍需 Apple 公证才能安全地对外分发；ad-hoc 签名包不应作为公开 Release。

### 通过终端转录

App 内的同一个可执行文件也提供无界面命令。以下示例先用完整路径；如果已将 App
放入 `/Applications`，可以按实际位置修改：

```bash
CLI='/Applications/bi2read.app/Contents/MacOS/bi2read'
"$CLI" --help
"$CLI" transcribe 'https://www.bilibili.com/video/BV...' --language zh
```

命令会读取与桌面界面相同的设置，等待下载、转码、转录和文档生成全部完成，然后
在标准输出中返回最终 `full.md` 的路径。首次使用前可在桌面界面安装 Runtime，也
可以完全通过终端完成：

```bash
"$CLI" runtime status
"$CLI" runtime status --json
"$CLI" runtime install --runtime-data-dir /Volumes/Data/bi2read-Runtime
```

需要把大文件明确放到外挂盘，或让 Agent 读取结构化结果时，可以运行：

```bash
"$CLI" transcribe 'BV...' \
  --work-dir /Volumes/Data/bi2read-Jobs \
  --output-dir /Volumes/Data/bi2read-Markdown \
  --runtime-data-dir /Volumes/Data/bi2read-Runtime \
  --json
```

使用 `"$CLI" <命令> --help` 可查看每个命令的完整参数。CLI 和桌面界面共享任务
状态及单实例锁；运行终端任务前请退出桌面 App，同一时间只运行一个实例。

## 首次配置

启动后打开“设置”。工作目录与 Markdown 输出目录已经使用 macOS 平台默认值，
也可以通过原生目录选择器修改：

1. **工作目录**：默认位于应用数据目录的 `Jobs`，保存任务中间产物。
2. **Markdown 输出目录**：默认为 `~/Documents/bi2read`。
3. **FunASR Runtime 项目目录**：本机构建的 App 会自动使用内置 Runtime；通过
   Cargo 运行或需要 Docker 后端时，可在此选择自定义 Runtime v2 项目。旧 v1 Runtime
   可以查看状态，但创建任务前必须升级。
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
3. 在“确认转写设置”中查看 Runtime 说明，选择中文、英文或 Runtime 明确提供的自动检测，确认后才加入队列。
4. 等待下载、转码、转录和文档生成完成，并在任务信息中核对请求/实际语言和 Runtime identity。
5. v0.5.0 新任务进入终态后可点击“查看消费结果”，在摘要、章节、忠实正文和原始稿之间切换；
   可核对来源时展开片段并跳回视频，来源受限时不会显示伪造链接。
6. 在任务详情中检查识别出的说话人；需要时修改显示名称，然后打开唯一最终文稿，或在 Finder
   中显示输出目录。

修改说话人名称后，bi2read 会直接重建 Markdown，不会重复转录音频。

## 输出文件

每个任务可能生成：

- `transcript.raw.json`：包含时间戳和说话人编号的结构化转录结果
- `transcript.raw.md`：按原始识别段落生成的 Markdown
- `transcript.readable.md`：整理后的可读正文
- `full.md`：包含视频信息、说话人和可点击时间链接的最终文稿
- `content-current.v1.json`：v0.5.0 新任务的唯一结构化结果事实，供结果视图和文档重建使用

v0.5.0 新任务保留单一最终 `full.md` 出口；legacy 任务继续使用兼容路径。具体保留哪些中间
文件由任务的保留策略决定。

## 当前限制

- 真实视频的识别质量会随音频、领域词汇和 Runtime 模型变化；v0.4.0 使用既有英文视频
  转写作为 Docker 回归参考，中文不设置正确率 benchmark，只验证契约、结构和失败行为。
  英文 profile 按既定方案使用 `paraformer-en`，不以其他模型的实验结果替代该验证。
  这不等同于对所有领域或语言组合做出准确率承诺。现有 AI 润色主要整理标点、明显错字
  和分段，不等同于已经验证的全文纠错。
- v0.5.0 可信文字消费结果入口只对带 marker、Evidence 有效且已进入终态的新任务开放，
  旧任务保持兼容行为。
- 暂不提供官方签名并公证的 DMG；可以运行上方脚本，在自己的 Apple Silicon Mac
  上生成 ad-hoc 签名的 `.app`。
- 仅支持匿名访问，不支持需要登录或 Cookie 的视频。
- 截图提取功能尚未开放。
- FFmpeg 仍需单独安装；自构建 App 已内置 Runtime 与 uv，Docker 只用于可选后端。
- Docker 后端的 FunASR 模型集合占用较多内存。`auto` 语言会先加载语言检测模型再加载
  识别、VAD 与标点模型；若任务以“Runtime 在转写时内存不足”失败，请为 Docker Desktop
  分配更多内存，或为该任务选择明确的中文/英文语言后重试。Docker 引擎无响应时任务会在
  探测失败后进入“等待用户操作”，不会永久停在运行中。每次转写的退出码与失败分类记录在
  任务目录的 `logs/runtime-exit.json`。
- 转写进行中 App 被退出或崩溃时，重启后该任务标记为失败并提供重试，不会自动重放，
  以免再次耗尽内存；需要时请手动重试。
- 任务冻结的 Runtime 被更换后，该任务无法直接重试，会进入“等待用户操作”；按提示重建
  任务即可使用当前 Runtime 重新转写。
- 内置原生 Runtime v2.0.0 的英文结果按 VAD 段输出，可能缺少句末标点；英文按句分段需要
  后续 Runtime 版本。

## 许可证

[MIT License](LICENSE)

<details>
<summary>English</summary>

## About

bi2read is a local macOS desktop application that turns Bilibili videos into
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
- v0.6.0 smart text curation for terminal jobs: AI connection test, model changed notice & regeneration, short/standard/long summary tiers, single-job text and speaker search, and versioned reading time estimation

### Requirements

- macOS
- Rust 1.92 or newer; `rust-toolchain.toml` pins the contributor toolchain to
  1.95, while 1.92 in `Cargo.toml` remains the MSRV
- FFmpeg available on `PATH`; Finder-launched App builds also check the standard
  `~/.local/bin`, Homebrew, and MacPorts locations
- uv when running the native runtime through Cargo; the app build script
  downloads it automatically
- bi2read FunASR Runtime contract v2 (contract v1 remains readable but cannot create new jobs)

FunASR models are not bundled. Docker Desktop is required only for the optional
Docker runtime.

### Install and run

[`Cargo.toml`](Cargo.toml) is the single source of truth for the app version.
See [`docs/releases/v0.6.1.md`](docs/releases/v0.6.1.md) for the current bilingual
release notes, compatibility details, and limitations.

```bash
git clone https://github.com/xiaozhenliu/bi2read.git
cd bi2read
cargo run --release
```

Download the verified native runtime separately:

```bash
git clone --branch v2.0.0 --depth 1 https://github.com/xiaozhenliu/bi2read-funasr-runtime.git
```

### Build the macOS app locally

With Rust, Xcode Command Line Tools, Git, and network access installed, run:

```bash
git clone https://github.com/xiaozhenliu/bi2read.git
cd bi2read
scripts/build-macos-local.sh
```

The script downloads pinned Runtime and uv releases, builds the app, and asks
for a signing identity. Press Enter for free ad-hoc signing suitable for use on
the same Mac. The app is written to
`target/macos-local/output/bi2read.app` by default. Set an absolute
`BI2READ_BUILD_ROOT` to build on another disk.

To use your own Developer ID, set `BI2READ_SIGN_IDENTITY` before running the
script. Developer ID builds still require Apple notarization before public
distribution. Do not publish the ad-hoc signed build as a Release asset.

### Transcribe from the terminal

The executable inside the app also provides a non-interactive CLI:

```bash
CLI='/Applications/bi2read.app/Contents/MacOS/bi2read'
"$CLI" --help
"$CLI" transcribe 'https://www.bilibili.com/video/BV...' --language en
```

It uses the same saved settings as the desktop app, waits for the production
pipeline to finish, and prints the final `full.md` path. Runtime setup is also
available without opening the GUI:

```bash
"$CLI" runtime status
"$CLI" runtime status --json
"$CLI" runtime install --runtime-data-dir /Volumes/Data/bi2read-Runtime
```

Use `--work-dir`, `--output-dir`, and `--runtime-data-dir` to keep large data on
another disk. Add `--json` for machine-readable output. Run
`"$CLI" <command> --help` for all options. The CLI and desktop app share state
and a single-instance lock, so quit the desktop app before starting a terminal
job.

### First-time setup

The locally built app discovers its bundled FunASR Runtime automatically. When
running through Cargo or using Docker, select a Runtime v2 project in
Settings. Older v1 Runtimes remain inspectable but must be upgraded before creating
a new job. Choose a Runtime Data directory with several gigabytes of free space;
it may be on an external drive. Install or validate the runtime from Settings. Job data defaults
to the macOS Application Support directory, while Markdown defaults to
`~/Documents/bi2read`; both locations can be changed with the native folder
picker. Choose a retention policy for intermediate files.
Local LLM refinement is optional; authenticated remote LLM services are not
supported in this release.

### Basic workflow

1. Make sure Settings reports that the runtime is ready. Start Docker Desktop
   only when using the Docker backend.
2. Paste a Bilibili link or video ID and review the Runtime/language confirmation.
3. Confirm the language choice before adding the job to the queue.
4. Wait for download, conversion, transcription, and document generation; verify requested/reported language and Runtime identity in task information.
5. On a v0.5.0 new job, open "查看消费结果" after it reaches a terminal state. Switch between
   summary, chapters, faithful text, and raw transcript; expand mapped sources or see the explicit limited-source state.
6. Review or rename detected speakers, then open the single final `full.md` document or reveal it in Finder.

### Output

Depending on the retention policy, a task may produce structured transcription
JSON, raw Markdown, refined Markdown, and a final `full.md` document. v0.5.0 new jobs also keep
`content-current.v1.json` as the single structured content fact and expose only one final `full.md` output.

### Current limitations

- Recognition quality varies with audio, domain vocabulary, and the Runtime
  model. v0.4.0 uses the existing English video transcription as a Docker
  regression reference and keeps `paraformer-en` as the explicit English
  profile; Chinese has contract/structure checks rather than an accuracy
  benchmark. This is not an accuracy guarantee for every domain or language
  combination. The current AI refinement mainly adjusts punctuation,
  obvious typos, and paragraphing, and is not a validated full-transcript
  correction system.
- Trusted text consumption is terminal-only for new jobs with valid Evidence;
  legacy jobs keep their compatibility path.
- No officially signed and notarized DMG is provided yet. Run the script above
  to create an ad-hoc signed `.app` on your own Apple Silicon Mac.
- Videos requiring login or cookies are not supported.
- Screenshot extraction is not available yet.
- FFmpeg must still be installed separately. Locally built apps bundle Runtime
  and uv; Docker remains optional.
- The Docker FunASR model set needs a lot of memory. `auto` loads the language
  detector before the ASR, VAD, and punctuation models. If a job fails with
  "Runtime ran out of memory", give Docker Desktop more memory or retry the job
  with an explicit Chinese/English language. When the Docker engine stops
  responding, the job moves to "needs user action" after the probe fails
  instead of staying in "running" forever. Each transcription records its exit
  code and failure classification in `logs/runtime-exit.json` in the job
  directory.
- If the app quits or crashes during transcription, the job is marked failed
  with a retry action after restart instead of being replayed automatically, so
  an out-of-memory run is not repeated; retry it manually when appropriate.
- After the Runtime a job was frozen to is replaced, the job cannot be retried
  directly and moves to "needs user action"; rebuild it to transcribe with the
  current Runtime.
- English output from the bundled native Runtime v2.0.0 follows VAD segments and
  may lack sentence punctuation; sentence-level English segmentation needs a
  later Runtime release.

### License

[MIT License](LICENSE)

</details>
