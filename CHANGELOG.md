# Changelog / 变更日志

本文件保存每个版本的简要摘要；完整说明见 `docs/releases/`。
This file keeps short per-version summaries; see `docs/releases/` for details.

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

