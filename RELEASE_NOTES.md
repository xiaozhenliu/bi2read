# BiMyScribe v0.2.0

本版本加入可复现的 macOS 本机构建流程。用户无需 Apple Developer 会员，即可从
源码生成适合当前机器自用的 ad-hoc 签名 `BiMyScribe.app`。

## 主要变化

- 新增 `scripts/build-macos-local.sh`，一条命令下载固定版本依赖、构建并签名 App。
- App 内置 FunASR Runtime v1.0.0 与 uv 0.11.23，并自动发现内置 Runtime。
- 自定义原生 Runtime 与 Docker Runtime 仍可在设置中选择，并优先于内置 Runtime。
- Python 环境、模型和缓存继续保存在可配置的 Runtime 数据目录，不写入 App 包。
- 支持通过环境变量选择 Developer ID；脚本也会在签名前提供交互提示。

## 安装

本 Release 不提供官方签名并经 Apple 公证的 DMG。请在 Apple Silicon Mac 上从
源码运行：

```bash
scripts/build-macos-local.sh
```

直接回车可使用免费的 ad-hoc 签名。生成的 App 适合在构建它的 Mac 上自用，不应
作为面向其他用户的 Release 安装包。FFmpeg 仍需单独安装并位于 `PATH`。

## 兼容性与升级

- 当前仅支持 Apple Silicon macOS。
- 已有设置、任务数据和 Markdown 输出不会因升级而删除。
- Runtime 指纹变化时，应用可能要求重新安装或验证 Runtime。
- Docker 仍是可选后端，不是运行本机构建 App 的必要条件。

## English

BiMyScribe v0.2.0 adds a reproducible local macOS app build. The new
`scripts/build-macos-local.sh` command downloads pinned Runtime v1.0.0 and uv
0.11.23 releases, assembles `BiMyScribe.app`, and prompts for signing. Press
Enter for free ad-hoc signing suitable for use on the Mac that built the app.

The app automatically discovers its bundled Runtime. Python environments,
models, and caches remain in the configurable Runtime Data directory. Custom
native and Docker runtimes are still supported and take precedence when set.

This source release does not include an officially signed and Apple-notarized
DMG. Apple Silicon macOS and FFmpeg on `PATH` are currently required.

BiMyScribe is licensed under the MIT License.
