# BiMyScribe 架构

版本：1.1
状态：当前稳定架构，持续演进  
产品基线：v0.3.0  
更新日期：2026-08-18

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

## 执行流

```text
GUI (`desktop`) ─┐
                 ├─> Scheduler ─> Pipeline ─> Bilibili / FFmpeg / FunASR
CLI (`cli`) ─────┘                      │
                                       ├─> raw transcript Evidence
                                       ├─> deterministic readable document
                                       ├─> optional LLM correction
                                       └─> final Markdown + cleanup

Job snapshot ─> UI bridge ─> generated Slint view models ─> `ui/**`
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
认证。v1 可以查询但不能创建 v0.4.0 新任务。`run` 只接受冻结 selection，先校验 contract、
ready 和 identity，再由两个 adapter 传递同一 requested language。normalized 结果经过结构化
校验后返回 `TranscribeOutcome`，Pipeline 把 Runtime 回报写入 `Job.transcription_result`。

当前 Docker FunASR Runtime v2 的语言 profile 是：`zh` 使用 `paraformer-zh`，`en` 使用
`paraformer-en`；在 FunASR 1.4.1 中，英文 profile 映射到
`damo/speech_paraformer_asr-en-16k-vocab4199-pytorch`，避免默认 ModelScope alias 指向不适合
英文的权重。`auto` 只使用 `iic/SenseVoiceSmall` 检测语言 token，随后路由到对应 Paraformer
profile，检测失败不回退。英文 profile 传递 `en_post_proc=true` 并保留 VAD、标点和 speaker
适配器组合；Runtime 适配器同时接受 FunASR 的 `text` 与 `sentence` 字段、过滤控制 token，
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
| `src/pipeline.rs` | `run_job` | 阶段顺序、恢复、产物校验、持久化、清理和增强回退 |
| `src/ui_bridge.rs` | crate 内进度/快照映射 | `JobViewSnapshot` 到 Slint row/detail/stage models 的全部转换 |
| `src/jobs.rs` | `Job`、`Queue`、snapshot 与 artifact validation | 状态机、原子写入、恢复和能力计算 |
| `src/bilibili.rs` | parse/fetch/download | 匿名 API、短链解析、DASH audio 选择和 retry |
| `src/funasr.rs` | `describe_runtime`、`run(TranscribeRequest)` | v1/v2 manifest、fingerprint/readiness、语言 argv、normalized 解析和资源回收 |
| `src/llm.rs` | `refine_utterances` | 协议 adapter、分批请求、响应校验和逐条回退 |
| `src/document.rs` | raw/readable/final render | 时间链接、说话人映射和 Markdown 组合 |
| `src/process.rs` | subprocess spawn/run/cancel | 进程组、日志、终止升级和 Docker cleanup |
| `src/config.rs`、`src/paths.rs` | 配置与平台路径 | 默认值、迁移、单实例锁、release-check 隔离 |

这里的“模块”以接口和职责定义，不以文件大小定义。新的拆分必须通过删除测试：删掉模块后，
复杂度会重新散落到多个 caller；只转发一次调用的文件不构成有价值的模块。

## 转写与 LLM 的事实边界

1. `funasr::run` 返回带 id、时间范围、speaker id 和文本的 utterance。
2. Pipeline 原子保存原始 JSON，并用 `document` 生成逐片段原始 Markdown。
3. 未启用 LLM 时，`document::render_readable_body` 只确定性合并相邻同 speaker 片段。
4. 启用 LLM 时，`llm::refine_utterances` 每 20 条调用一次所选协议 adapter；返回值仍与
   输入一一对应。请求、解析或校验异常时使用原文并记录非致命 warning。
5. FinalDocument 组合元数据、时间链接和 readable body；Cleanup 按任务冻结的保留策略执行。

当前 LLM seam 有两个真实 protocol adapter（OpenAI Chat Completions 与 Anthropic Messages），
因此协议变化位于真实 seam。标题上下文、长文滑窗、术语表、provider 鉴权和质量评测尚未
进入该接口，不能由文档假设为已有能力。

## 错误与回退

- metadata、下载、FFmpeg、FunASR、I/O 和最终文档失败是 fatal，任务保留失败阶段供重试。
- Docker backend 未运行进入可操作等待状态，不 busy-loop。
- LLM 是 optional enhancement：失败记录 `LlmFallback`，任务继续生成规则阅读稿。
- Screenshots 未实现时阶段明确为 `Skipped`，不能显示为已完成产物。
- 恢复只跳过“状态完成且产物校验有效”的阶段；保留策略预期删除的产物按明确规则处理。

## 变更路由

- 新来源或下载行为进入 `bilibili`；不要放进 UI controller。
- ASR backend/contract 进入 `funasr`；Pipeline 只消费统一 utterance。
- 文本校正协议进入 `llm` adapter；忠实性和来源映射由其外部接口共同约束。
- 新派生产物先定义 Evidence 输入、独立失败语义和可重跑身份，再接入 Pipeline。
- Job 状态先在 `jobs` 建模，再通过 `ui_bridge` 展示；Slint 不持久化业务事实。
- 只有第二个真实 adapter 或测试替身确有必要时才新增 seam；否则把变化留在现有深模块内部。
