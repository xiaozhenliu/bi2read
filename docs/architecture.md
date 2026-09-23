# bi2read 架构

版本：1.3
状态：当前稳定架构，持续演进
产品基线：v0.6.0
更新日期：2026-08-25

本文描述当前稳定实现与模块接口。当前优先级见 `roadmap.md`；长期产品需求由内部需求
基线维护，历史方案不参与架构裁决。

## 核心不变量

- `transcript.raw.json` 与 `transcript.raw.md` 是 ASR Evidence；LLM 不覆盖它们。
- 规则阅读稿和 AI 校正稿都是可重建的派生产物，失败不得阻止可信基础产物完成。
- `Job`、阶段状态与产物路径先持久化，再由 GUI/CLI 映射；界面不从颜色或列表位置反推状态。
- 一个进程实例串行执行队列；取消必须到达子进程组和本次任务拥有的 Runtime 资源。
- Runtime source、backend、contract 和冻结输入是不同概念，不用路径猜测替代显式校验。
- 新任务的 `TranscriptionSelection` 在创建时冻结 Runtime source、绝对路径、ready
  fingerprint identity、backend、模型说明和 `SourceLanguage`；旧任务缺失该字段只标记
  `legacy-unrecorded`，不得从当前 Config 回填。
- Runtime contract v1 可以描述和展示，但新任务与执行入口必须使用 v2；v2 的 requested language
  通过 native-uv 与 Docker adapters 的同一 `--language auto|zh|en` 语义传递。
- Runtime normalized 输出先经过 schema、segments、文本和时间范围校验，成功后才写
  `transcript.raw.json`；reported language/model/identity 缺失时保持“未报告”。
- v0.5 新 Job 由 `content_setup` marker 识别；`ContentResults` 以同一份经过验证的
  Evidence 生成并校验各项文字结果，持久化唯一 `content-current.v1.json`（Schema v2）。
- v0.5 结果入口只对 Evidence 有效的终态 Job 开放；重生成与 RawOnly 修复只在当前进程内串行，
  不写持久命令、历史或恢复队列。
- v0.5/v0.6 App 与文档都消费结构化 current；Presentation 只有一个最终 `full.md` 出口，legacy Job
  完全 passthrough，不迁移、不从旧 Markdown 提升内容事实。

## 执行流

```text
GUI (`desktop`) ─┐
                 ├─> Scheduler ─> Pipeline ─> Bilibili / FFmpeg / FunASR
CLI (`cli`) ─────┘                      │
                                       ├─> raw transcript Evidence
                                       ├─> ContentResults (v0.5+ marker jobs)
                                       │       ├─> content-current.v1.json (Schema v2)
                                       │       └─> validated result snapshot
                                       ├─> document Presentation
                                       │       └─> one final full.md
                                       └─> cleanup

Job snapshot ─> UI bridge ─> generated Slint view models ─> `ui/**`
ContentResults current ─> result view / source expansion / regeneration / search / reading time
```

GUI 和 CLI 共享 `Scheduler`、`Pipeline`、`Job` 与产物契约，不能各自发展一条流水线。

## v0.4.0 Runtime 与语言选择

新任务的创建链路是：

```text
Config defaults -> funasr::describe_runtime -> JobCreationInput
               -> Job::from_creation -> Queue/state.json
```

App 在确认框中展示 Runtime 说明和语言选择，确认前不写队列；CLI 直接接受
`--language auto|zh|en`，省略时显式使用 `auto`。两条路径都通过同一个 Job 构造 seam
冻结 selection。设置变化只影响后续新任务，普通重试和恢复只消费 Job 中的 selection。

`funasr::describe_runtime` 是 Runtime 说明的唯一入口，负责规范化项目路径、读取 v1/v2
manifest、计算内容 fingerprint、匹配 ready record 并传播可选说明字段；说明不被当作能力
认证。v1 可以查询但不能创建新任务。`run` 只接受冻结 selection，先校验 contract、
ready 和 identity，再由两个 adapter 传递同一 requested language。normalized 结果经过结构化
校验后返回 `TranscribeOutcome`，Pipeline 把 Runtime 回报写入 `Job.transcription_result`。

当前 Docker FunASR Runtime v2 的语言 profile 是：`zh` 使用 `paraformer-zh`，`en` 使用
`paraformer-en`；在 FunASR 1.4.1 中，英文 profile 映射到
`damo/speech_paraformer_asr-en-16k-vocab4199-pytorch`，避免默认 ModelScope alias 指向不适合
英文的权重。`auto` 只使用 `iic/SenseVoiceSmall` 检测语言 token，随后路由到对应 Paraformer
profile，检测失败不回退。英文 profile 传递 `en_post_proc=true` 并保留 VAD、标点和 speaker
适配器组合。`paraformer-en` 不输出词级时间戳，FunASR 因此把 `sentence_info` 回退为未加标点
的 VAD 段；Runtime 在英文路径下改为消费顶层已加标点的整段文本，按句末标点切句，并用 VAD 段的
时间范围按字符比例回填每句起止时间（保证单调且覆盖段端点），speaker 继承句子中点所在 VAD 段的
说话人。zh 路径仍直接使用
带时间戳的 `sentence_info`。Runtime 适配器同时接受 FunASR 的 `text` 与 `sentence` 字段、过滤控制 token，
并在写 Evidence 前拒绝空文本、空 segments、未知 schema、倒置或越界时间和不匹配 identity。

CLI 的 `--json` 与 `runtime status --json` 统一返回 `CliEnvelope<T>`（schema version 1）；
stdout 只承载一个 envelope，进度和诊断写入 stderr。桌面任务信息从 Job snapshot 只读展示
requested/reported language、model 与 Runtime identity。

## 模块地图

| 模块 | 小接口 | 隐藏的实现 |
| --- | --- | --- |
| `src/main.rs` | 进程模式路由 | 只选择 package self-check、CLI 或桌面应用 |
| `src/desktop.rs` | `desktop::run` | 单实例、恢复、Slint models、controller callbacks、worker 生命周期 |
| `src/package_check.rs` | `package_check::run` | 包内 Runtime、uv、release manifest、身份和哈希校验 |
| `src/cli.rs` | `dispatch_requested` | CLI 参数、路径覆盖、Runtime 命令、版本化 envelope 和退出码 |
| `src/scheduler.rs` | `Scheduler::drain_next` | runnable 选择、取消 registry、任务终态收敛 |
| `src/pipeline.rs` | `run_job` | 阶段顺序、Evidence 持久化、ContentResults 初始执行、产物校验、清理和终态收敛 |
| `src/content_results.rs` | `current`、`execute`、`test_connection`、`search` | Evidence 校验、三档摘要、忠实正文、章节、重点、来源映射、失败隔离、单视频搜索与 Schema v2 发布 |
| `src/reading_time.rs` | `estimate_reading_time`、`measure_body` | 中/英/混排字数规模测量、版本化 profile（v1）与原片节省时长计算 |
| `src/ui_bridge.rs` | crate 内进度/快照映射 | `JobViewSnapshot` 到 Slint row/detail/stage models 的全部转换 |
| `src/jobs.rs` | `Job`、`Queue`、snapshot 与 artifact validation | 状态机、原子写入、恢复和能力计算 |
| `src/bilibili.rs` | parse/fetch/download | 匿名 API、短链解析、DASH audio 选择和 retry |
| `src/funasr.rs` | `describe_runtime`、`run(TranscribeRequest)` | v1/v2 manifest、fingerprint/readiness、语言 argv、normalized 解析和资源回收 |
| `src/llm.rs` | `refine_utterances` | 协议 adapter、分批请求、响应校验和逐条回退 |
| `src/document.rs` | raw/readable/final render | 基于 ContentSnapshot 的原始稿、faithful/readable 和按档位导出的唯一 `full.md` Presentation |
| `src/process.rs` | subprocess spawn/run/cancel | 进程组、日志、可执行文件发现、终止升级和 Docker cleanup |
| `src/config.rs`、`src/paths.rs` | 配置与平台路径 | 默认值、迁移、单实例锁、release-check 隔离 |

这里的“模块”以接口和职责定义，不以文件大小定义。新的拆分必须通过删除测试：删掉模块后，
复杂度会重新散落到多个 caller；只转发一次调用的文件不构成有价值的模块。

## v0.5/v0.6 可信文字消费与智能精选

`ContentResults` 是文字消费核心深模块，外部暴露 `current(job)`、
`execute(job, intent, cancel)`、`test_connection(target)` 以及查询解析纯函数。模块内部完成
raw Evidence 定位与 schema/SHA 校验、上下文快照、faithful/chapters/short summary/standard summary/long summary/highlights
各项派生、mapped/limited 来源状态、失败隔离与 `content-current.v1.json`（Schema v2）整文件原子发布。

- **三档摘要（v0.6）**：short（≤200 字）、standard（300–500 字）、long（800–1500 字）拥有独立契约；未生成档位回退 FaithfulText；导出严格按选定档位输出。
- **连接探测（v0.6）**：10 秒超时映射五类结构化故障；Slint 与 controller 间使用单调递增 `probe_token` 实现严格的草稿变动失效。
- **单视频搜索（v0.6）**：覆盖忠实正文与原始稿双层匹配，支持 `mm:ss` 时间点（±30 秒）、时间范围和说话人过滤。
- **阅读时间（v0.6）**：由 `src/reading_time.rs` 纯函数计算，中/英/混排字数规模经版本化 profile（v1）输出一致的阅读时间与节省时长。

`current()` 只读并返回 `Legacy`、`RawOnly` 或 `Current`：marker 缺失时是 Legacy；current
缺失或损坏但 Evidence 有效时是 RawOnly；其余情况返回带 validated Evidence 的 Current。它不从
Markdown 修复，也不会因普通打开动作写盘。

初始执行由 Pipeline 在 Evidence 完成后调用；结果视图只在终态 Job 展示。终态 v0.5 Job 可从
既有 Evidence 单项重生成章节、默认摘要或重点，也可对 RawOnly 执行一次 Initial 修复。两者共享
单进程、单 worker 的内存 busy 门禁；旧 current 在失败时保留，进程退出后不自动续跑，Job 的
Completed/Failed/Cancelled、stage 和 finished time 不被改写。界面不暴露单项取消、持久队列或
历史/投影术语。

App 直接消费 `ContentSnapshotV1`；`document` 使用同一 snapshot 重建 `transcript.raw.md`、
`transcript.readable.md` 和唯一最终 `full.md`。打开/导出前重建失败时不打开旧 `full.md`，结果页
仍保留可读内容。缺少 marker 的 legacy Job 不生成 current、不迁移、不从展示文件反推结构化内容，
继续 v0.4.1 的 open/reveal/retry/speaker rename 兼容路径。

## 转写与 LLM 的事实边界

1. `funasr::run` 返回带 id、时间范围、speaker id 和文本的 utterance。
2. Pipeline 原子保存原始 JSON，并用 `document` 生成逐片段原始 Markdown。
3. 未启用 LLM 时，`document::render_readable_body` 只确定性合并相邻同 speaker 片段。
4. legacy Job 启用 LLM 时，`llm::refine_utterances` 每 20 条调用一次所选协议 adapter；返回值仍
   与输入一一对应。请求、解析或校验异常时使用原文并记录非致命 warning。v0.5 Job 不走这条
   旧的逐条派生路径，而由 `ContentResults` 统一裁决 faithful fallback 与三个增强 slot。
5. `document` 组合元数据、时间链接和结构化结果；v0.5 的 Presentation 只写一个最终 `full.md`，
   Cleanup 按任务冻结的保留策略执行。

当前 LLM seam 有两个真实 protocol adapter（OpenAI Chat Completions 与 Anthropic Messages），
因此协议变化位于真实 seam。标题上下文、长文滑窗、术语表、provider 鉴权和质量评测尚未
进入该接口，不能由文档假设为已有能力。

## 错误与回退

- metadata、下载、FFmpeg、FunASR、I/O 和最终文档失败是 fatal，任务保留失败阶段供重试。
- Docker backend 未运行进入可操作等待状态，不 busy-loop。Docker 健康检查是有超时（10 s）
  的 `docker version`，socket 存在但引擎无响应同样视为不可用。
- Docker 转写容器不使用 `--rm`：Runtime 退出后 App 先 `docker inspect` 读取 `OOMKilled`
  与退出码，再删除自有标签的容器。退出码 137 或 `OOMKilled` 归类为
  `runtime-out-of-memory`；等待期间每 5 s 探测引擎，连续 3 次失联归类为
  Docker 不可用；总时长超过 `max(60 min, 10 × 音频时长)` 归类为 `runtime-timeout`。
  每次终结把 `{exit_code, oom_killed, docker_reachable, classification}` 写入任务目录
  `logs/runtime-exit.json`。
- 转写阶段发现任务冻结的 Runtime 身份与当前 Runtime 失配时，进入等待用户操作并持久化
  `runtime-identity-changed` 错误码；该错误码开放“重建”入口（用当前 Runtime 以同一 URL
  新建任务，原任务保持不变），重试不会再次尝试失配的 Runtime。
- 启动恢复时，转写阶段处于 Running 的任务判定为“被中断”，进入 Failed 并保留 Retry；
  不自动重排队，避免重放 OOM。其他阶段的 Running 仍重置为 Pending 并自动续跑。
- LLM 是 optional enhancement：失败记录 `LlmFallback`，任务继续生成规则阅读稿。
- v0.5 增强失败只更新对应 slot 的 `last_failure`；faithful AI 失败使用可追溯的规则 fallback，
  不遮蔽 Evidence 或其他 current。
- v0.5 current 缺失/损坏但 Evidence 有效时显示 RawOnly 与“修复文字结果”；Evidence 无效时不显示
  结果入口，要求重新创建任务。
- 重生成与修复不创建持久 operation、attempt、历史或 projection ledger；关闭 App 后不自动续跑。
- Screenshots 未实现时阶段明确为 `Skipped`，不能显示为已完成产物。
- 恢复只跳过“状态完成且产物校验有效”的阶段；保留策略预期删除的产物按明确规则处理。

## 变更路由

- 新来源或下载行为进入 `bilibili`；不要放进 UI controller。
- ASR backend/contract 进入 `funasr`；Pipeline 只消费统一 utterance。
- 文本校正协议进入 `llm` adapter；忠实性和来源映射由其外部接口共同约束。
- 新派生产物先定义 Evidence 输入、独立失败语义和可重跑身份，再接入 Pipeline；v0.5 派生必须
  继续收敛在 `ContentResults`，不要在 UI、Pipeline 或 document 中复制规则。
- Job 状态先在 `jobs` 建模，再通过 `ui_bridge` 展示；Slint 不持久化业务事实。
- 只有第二个真实 adapter 或测试替身确有必要时才新增 seam；否则把变化留在现有深模块内部。
