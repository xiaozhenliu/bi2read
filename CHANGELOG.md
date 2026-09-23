# Changelog / 变更日志

本文件保存每个版本的简要摘要；完整说明见 `docs/releases/`。
This file keeps short per-version summaries; see `docs/releases/` for details.

## Unreleased

### 中文

- 暂无。

### English

- None.

## 0.6.1 - 2026-09-23

### 中文

- 产品更名为 bi2read：App、CLI、bundle 标识与公开仓库统一改名；首次启动把旧
  `Application Support/BiMyScribe` 状态目录一次性迁移到 `bi2read`，用户自选路径与已有文稿不移动。
- 界面视觉统一：B 站蓝语义配色、白色输入框、18/14/12px 字号、队列信息分层与说话人列表单一收尾线。
- 音频流水线恢复：Finder 启动时也能找到常见位置的 FFmpeg/ffprobe，音频原子下载并拒绝截断响应，
  多分 P 使用所选分 P 时长。
- Runtime 内存耗尽与超时收敛：区分 `runtime-out-of-memory` 与 `runtime-timeout`，检测 Docker 引擎失联，
  写入 `logs/runtime-exit.json`；重启时被中断的转写标记为失败并可重试，不再自动重放。
- 任务冻结的 Runtime identity 改变后进入“等待用户操作”并提供重建入口；详情面板实时同步阶段与进度。
- `transcribe --json` 的 stdout 只输出 JSON；打包自检与发布脚本兼容新旧 Runtime manifest 文件名，
  内置 Runtime v2.0.0 继续回报实际处理身份。Runtime 固定输入保持 v2.0.0。

### English

- Renamed to bi2read across the app, CLI, bundle identifier, and public repository; the legacy
  `Application Support/BiMyScribe` state directory moves to `bi2read` once on first launch, while user-selected
  paths and existing documents stay in place.
- Unified visuals: Bilibili-blue semantic colours, white inputs, an 18/14/12px type scale, layered queue
  information, and a single closing rule under the speaker list.
- Audio pipeline recovery: Finder-launched apps find FFmpeg/ffprobe in common locations, audio downloads are
  atomic and reject truncated responses, and multi-part videos use the selected part's duration.
- Runtime out-of-memory and timeout convergence: `runtime-out-of-memory` and `runtime-timeout` are distinct,
  Docker engine loss is detected, `logs/runtime-exit.json` is written, and an interrupted transcription is
  marked failed with retry instead of being replayed after restart.
- A job whose frozen Runtime identity changed moves to "needs user action" with a rebuild action; the detail
  pane follows live stage and progress updates.
- `transcribe --json` keeps stdout JSON-only; the package self-check and release scripts accept both Runtime
  manifest names, and bundled Runtime v2.0.0 keeps reporting the processing identity. Runtime inputs stay at
  v2.0.0.

## 0.6.0 - 2026-08-25

### 中文

- 新增 AI 连接测试（F1）：支持在设置中测试本地 OpenAI/Anthropic 兼容端点，10 秒超时映射，结构化诊断地址错误、模型不可用、协议不匹配、服务不可达或超时，并具备草稿状态隔离。
- 新增模型更换提示与重生成（F2）：当设置中的模型变更时在文字结果提示模型更换，支持针对各文字档位从既有 Evidence 重新生成并记录生成模型。
- 新增短/标准/长三档摘要与 Schema v2（F3）：支持在界面中切换短、标准、长三档摘要深度；三档具备独立长度与来源状态契约；未生成档位呈现平滑空状态与针对性生成按钮；选中档位未生成时直接回退忠实正文，`full.md` 严格按选定档位导出。
- 新增单视频文字搜索（F4）：支持在消费结果中搜索忠实正文与原始转写，支持时间点/时间范围与说话人过滤并实现精确跳转高亮。
- 新增版本化阅读时间估算（F5）：基于纯中、纯英及中英混排正文字数规模，提供版本化阅读时间与视频时长节省估算，并在界面中提供“如何计算”展开式依据说明。

### English

- Added AI Connection Test (F1): test local OpenAI/Anthropic-compatible endpoints in Settings with 10s deadline, classifying into InvalidAddress, ModelUnavailable, ProtocolMismatch, Unreachable, or Timeout.
- Added Model Changed Notice & Regeneration (F2): notify when configured model differs from latest generation record, supporting targeted slot regeneration without re-running ASR.
- Added Three-tier Summary & Schema v2 (F3): support Short, Standard, and Long summary tiers with independent length/source contracts, clean idle state for ungenerated tiers, direct fallback to faithful body text, and consistent tier export in full.md.
- Added Single-job Text Search (F4): filter across faithful text and raw transcripts with time range, timestamp jumping, and speaker filtering.
- Added Versioned Reading Time Estimation (F5): compute reading time and video time saved across Chinese, English, and mixed scripts with collapsible explainer accordion.

## 0.5.1 - 2026-08-24

### 中文

- 修复公开源码归档中 ADR 与历史 design-plan 指向内部排除文档的失效链接；改为公开可达的
  Design Spec 或不带链接的产品依据说明。产品行为与 Runtime 输入不变。
- 匿名 Runtime Release 门禁使用 GitHub 标准 API 请求头和唯一查询参数，避免边缘节点缓存的
  504 响应阻塞可用 Release 的核验。

### English

- Fixed broken links from public ADRs and the historical design-plan to excluded internal documents. References
  now use the public Design Spec or plain product-basis text. Product behavior and Runtime inputs are unchanged.
- The anonymous Runtime Release gate now uses standard GitHub API headers and a unique query parameter so a
  cached edge 504 cannot repeatedly block verification of an available release.

## 0.5.0 - 2026-08-24

### 中文

- 新增可信文字消费结果：终态新任务可在应用内查看忠实正文、章节、默认摘要、
  重点和原始稿，并区分可核对来源与来源受限状态。
- 结果由 `ContentResults` 从已验证 Evidence 生成，使用单一 `content-current.v1.json`；章节、
  默认摘要和重点可在终态单项重新生成，修复与重生成只保留在当前进程内，不改变任务终态。
- Presentation 只有一个最终 `full.md` 出口；缺少 marker 的旧任务继续兼容路径，
  不迁移、不从旧 Markdown 反推 v0.5 内容。

### English

- Trusted text consumption for terminal new jobs adds faithful text, chapters,
  a default summary, highlights, the raw transcript, and explicit mapped or limited source states.
- `ContentResults` derives the view from validated Evidence and persists one `content-current.v1.json`;
  chapters, the default summary, and highlights can be regenerated independently in memory without changing
  the Job terminal state.
- Presentation has one final `full.md` output. Jobs without the marker keep their legacy
  compatibility path and are not migrated.

## 0.4.1 - 2026-08-22

### 中文

- 新增转写历史管理：已完成、失败、已取消的任务可从详情页删除，任务队列提供
  “清空历史”一键删除全部已结束任务；排队与运行中的任务不受影响。
- 删除与清空均经过确认弹窗，移除任务记录的同时删除任务工作目录与专属输出
  目录；外置盘未连接视为产物已不存在，删除后重启不会复活任务。
- 删除选中任务后自动选中相邻任务，队列清空后回到空状态。

### English

- Transcription history management: completed, failed and cancelled jobs can be
  deleted from the task detail, and the queue header gains a "clear history"
  action that removes every finished job at once; queued and running jobs are
  untouched.
- Deletion and clearing require a confirmation dialog and remove the queue
  record together with the job's working directory and its own output
  directory; an unplugged external drive counts as already gone, and deleted
  jobs never reappear after a restart.
- Deleting the selected task selects the neighbouring task automatically, and
  clearing the last task returns the queue to its empty state.

## 0.4.0 - 2026-08-22

### 中文

- Runtime 契约升级到 v2：中文路由 paraformer-zh，英文路由 paraformer-en，auto 先用
  SenseVoiceSmall 检测再路由，检测失败不回退。
- 提交任务前新增转写确认流程，语言选择由用户显式做出并随任务冻结；重试、恢复与
  重跑不被当前设置改变，Runtime 身份变化在启动前明确失败。
- 转写结果回报实际使用的语言、模型与 Runtime 身份；v1 Runtime 创建新任务时返回
  稳定的“需要升级”错误。
- CLI 新增稳定 JSON 契约：统一信封、稳定错误码与退出码，适合 Agent 安全调用。
- 新增 fixture 驱动的全流水线自动化集成测试（成功路径、失败重试、断点恢复、语言
  冻结与保留策略）。

### English

- Runtime contract upgraded to v2: Chinese routes to paraformer-zh, English to
  paraformer-en, and auto detects with SenseVoiceSmall before routing without
  silent fallback.
- A transcription confirmation step freezes the explicit language choice with
  the job; retry, recovery and reruns are never overridden by current settings,
  and changed runtime identities fail loudly before launch.
- Transcription results report the language, model and runtime identity actually
  used; v1 runtimes return a stable upgrade-required error for new jobs.
- The CLI gained a stable JSON contract with one envelope, stable error codes
  and exit codes, safe for agent callers.
- Added fixture-driven full-pipeline integration tests covering happy paths,
  failure retry, crash recovery, language freeze and retention policies.

## 0.3.0 - 2026-08-16

### 中文

- 界面视觉层重做：B 站品牌粉主色、暖色中性色、卡片式任务队列、按阶段语义着色的任务状态。
- 任务详情新增状态徽章与阶段图标；检查器改为分段控件，说话人显示片段数。
- 焦点环、禁用文字与弱化文字提高对比度以满足 WCAG AA。

### English

- Restyled the interface around the Bilibili brand pink: warm neutrals, card-based
  task queue, and stage-semantic task state colours.
- Added a status badge and stage icons to task detail; the inspector uses a
  segmented control and shows per-speaker segment counts.
- Darkened focus rings, disabled text and muted text to meet WCAG AA.

## 0.2.2 - 2026-08-13

### 中文

- 确定性合并相邻同 Speaker 的转写片段，同时保留原始逐片段时间链接。
- 提供固定 Runtime v1.0.0 与 uv 0.11.23 的可复现本机 macOS 构建。

### English

- Deterministically group adjacent segments from the same speaker while preserving raw timestamp links.
- Provide a reproducible local macOS build pinned to Runtime v1.0.0 and uv 0.11.23.

## 0.2.1 - 2026-08-11

### 中文

- 增加可读任务目录名称与 macOS App 图标。

### English

- Added readable job directory names and a macOS app icon.

## 0.2.0

### 中文

- 首个 BiMyScribe 桌面应用版本。

### English

- Initial BiMyScribe desktop application release.
