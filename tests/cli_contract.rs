use std::process::Command;

fn run_cli(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bimyscribe"))
        .args(arguments)
        .output()
        .expect("bimyscribe binary should start")
}

fn json_object(output: &std::process::Output) -> serde_json::Value {
    assert!(
        !output.stdout.is_empty(),
        "stdout must contain the JSON envelope; stderr was: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be one JSON value");
    assert!(value.is_object(), "stdout JSON must be an object");
    value
}

fn setup_v1_release_root() -> (std::path::PathBuf, String) {
    let root = std::env::temp_dir().join(format!("bimyscribe-cli-v1-{}", uuid::Uuid::new_v4()));
    let token = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(".bimyscribe-release-check"), token).unwrap();
    let runtime = root.join("Runtime");
    std::fs::create_dir_all(runtime.join("schemas")).unwrap();
    std::fs::write(
        runtime.join("bimyscribe-runtime.toml"),
        "contract_version = 1\nbackend = \"native-uv\"\nentrypoint = \"transcribe.py\"\noutput_schema_version = 1\noutput_schema_file = \"schemas/normalized-v1.schema.json\"\n",
    )
    .unwrap();
    for file in [
        "transcribe.py",
        "pyproject.toml",
        ".python-version",
        "uv.lock",
        "schemas/normalized-v1.schema.json",
    ] {
        std::fs::write(runtime.join(file), "fixture").unwrap();
    }
    let data_dir = root.join("RuntimeData");
    std::fs::create_dir_all(&data_dir).unwrap();
    let initialized = run_cli(&[
        "--release-check-root",
        root.to_str().unwrap(),
        "--release-check-token",
        token,
        "config",
        "show",
    ]);
    assert!(
        initialized.status.success(),
        "state initialization failed: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let app_support = root.join("Library/Application Support/BiMyScribe");
    std::fs::write(
        app_support.join("config.toml"),
        format!(
            "runtime_project = {:?}\nruntime_data_dir = {:?}\n",
            runtime.to_string_lossy(),
            data_dir.to_string_lossy()
        ),
    )
    .unwrap();
    (root, token.into())
}

#[test]
fn invalid_language_returns_one_json_error_envelope_and_exit_two() {
    let output = run_cli(&["transcribe", "BV1HuMk61ECq", "--language", "fr", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let value = json_object(&output);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], false);
    assert_eq!(value["data"], serde_json::Value::Null);
    assert_eq!(value["error"]["code"], "invalid-arguments");
}

#[test]
fn transcribe_without_runtime_returns_one_json_runtime_error() {
    let root =
        std::env::temp_dir().join(format!("bimyscribe-cli-contract-{}", uuid::Uuid::new_v4()));
    let output = run_cli(&[
        "--release-check-root",
        root.to_str().unwrap(),
        "--release-check-token",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "transcribe",
        "BV1HuMk61ECq",
        "--language",
        "auto",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(3));
    let value = json_object(&output);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "runtime-not-configured");
    assert!(value["error"]["action"].is_string());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn runtime_status_json_without_runtime_is_one_json_runtime_error() {
    let root = std::env::temp_dir().join(format!("bimyscribe-cli-status-{}", uuid::Uuid::new_v4()));
    let output = run_cli(&[
        "--release-check-root",
        root.to_str().unwrap(),
        "--release-check-token",
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        "runtime",
        "status",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(3));
    let value = json_object(&output);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "runtime-not-configured");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn v1_runtime_status_is_machine_readable_but_transcribe_returns_upgrade_error() {
    let (root, token) = setup_v1_release_root();
    let status = run_cli(&[
        "--release-check-root",
        root.to_str().unwrap(),
        "--release-check-token",
        &token,
        "runtime",
        "status",
        "--json",
    ]);
    assert_eq!(
        status.status.code(),
        Some(3),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let status_json = json_object(&status);
    assert_eq!(status_json["ok"], true);
    assert_eq!(status_json["data"]["runtime"]["contract_version"], 1);
    assert_eq!(status_json["data"]["runtime"]["ready"], false);

    let transcribe = run_cli(&[
        "--release-check-root",
        root.to_str().unwrap(),
        "--release-check-token",
        &token,
        "transcribe",
        "BV1HuMk61ECq",
        "--language",
        "en",
        "--json",
    ]);
    assert_eq!(transcribe.status.code(), Some(3));
    let transcribe_json = json_object(&transcribe);
    assert_eq!(transcribe_json["ok"], false);
    assert_eq!(
        transcribe_json["error"]["code"],
        "runtime-contract-upgrade-required"
    );
    std::fs::remove_dir_all(root).ok();
}
