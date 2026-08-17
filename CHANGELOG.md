# Changelog / 变更日志

本文件保存每个版本的简要摘要；完整说明见 `docs/releases/`。
This file keeps short per-version summaries; see `docs/releases/` for details.

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

