# BiMyScribe v0.1.0

Initial open-source release candidate.

BiMyScribe is a lightweight local desktop application that turns Bilibili
videos into readable Markdown transcripts. It runs the media-processing and
speech-recognition pipeline locally and preserves speaker information produced
by FunASR.

## Highlights

- Add videos using Bilibili URLs, BV IDs, AV IDs, or `b23.tv` short links.
- Process multi-part videos through a persistent task queue.
- Download audio and normalize it to 16 kHz mono WAV with FFmpeg.
- Transcribe locally through a versioned FunASR Runtime with speaker separation.
- Preserve detected speaker labels and rename speakers from the desktop UI.
- Rebuild Markdown after speaker renaming without running transcription again.
- Generate raw transcripts and readable Markdown with clickable Bilibili
  timestamp links.
- Recover queued and interrupted work after restarting the application.
- Cancel active download, FFmpeg, and transcription subprocesses.
- Optionally refine readable output through a local, unauthenticated
  OpenAI-compatible LLM endpoint such as Ollama.
- Keep working output on a configured external drive with selectable retention
  policies.

## Requirements

- macOS
- Rust 1.92 or newer when building from source
- FFmpeg available on `PATH`
- uv for the default native Runtime, or Docker Desktop for the optional Docker backend
- [BiMyScribe FunASR Runtime v1.0.0](https://github.com/xiaozhenliu/bimyscribe-funasr-runtime/releases/tag/v1.0.0)
- A writable location for task data and Markdown output. Platform defaults are
  used automatically and can be changed in Settings.
- Several gigabytes for the Runtime environment and models; this directory can
  be placed on an external drive.

## Known limitations

- No packaged `.app` bundle is provided yet; run the application with Cargo.
- A second application instance exits without modifying persisted state.
- Screenshot extraction is currently skipped.
- Bilibili access is anonymous; videos requiring authentication are not
  supported.
- Remote LLM endpoints requiring authentication are not supported.
- FunASR setup and model assets are not bundled with the application.

## Build and run

```bash
cargo run --release
```

This release is licensed under the MIT License.
