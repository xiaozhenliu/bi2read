//! Packaged application integrity check.
//!
//! [`run`] validates the bundled Runtime, uv binary, release manifest, and
//! immutable package identities without exposing the verification sequence to
//! the process entry point.

#[derive(serde::Deserialize)]
struct PackageReleaseManifest {
    app_version: String,
    build_number: u64,
    public_revision: String,
    private_revision: String,
    runtime_tag: String,
    runtime_revision: String,
    runtime_backend: String,
    runtime_contract: u32,
    uv_version: String,
    target_platform: String,
    target_arch: String,
    binary_sha256: String,
    uv_sha256: String,
    runtime_manifest_sha256: String,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let runtime =
        crate::funasr::resolve_runtime_project(None).ok_or("bundled Runtime or uv is missing")?;
    let manifest = crate::funasr::load_runtime(&runtime)?;
    if manifest.backend != crate::funasr::RuntimeBackend::NativeUv {
        return Err("bundled Runtime backend must be native-uv".into());
    }
    let resources = runtime
        .parent()
        .ok_or("bundled Runtime has no Resources directory")?;
    let release: PackageReleaseManifest =
        serde_json::from_slice(&std::fs::read(resources.join("release-manifest.json"))?)?;
    let uv = resources.join("bin/uv");
    let uv_output = std::process::Command::new(&uv).arg("--version").output()?;
    if !uv_output.status.success() {
        return Err("bundled uv --version failed".into());
    }
    let actual_uv = String::from_utf8(uv_output.stdout)?
        .split_whitespace()
        .nth(1)
        .ok_or("bundled uv returned an invalid version")?
        .to_string();
    if release.app_version != env!("CARGO_PKG_VERSION")
        || release.build_number == 0
        || release.runtime_backend != "native-uv"
        || release.runtime_contract != manifest.contract_version
        || release.uv_version != actual_uv
        || release.target_platform != "macos"
        || release.target_arch != "arm64"
        || release.public_revision.len() != 40
        || release.private_revision.len() != 40
        || release.runtime_revision.len() != 40
        || release.runtime_tag.is_empty()
        // Signing changes the executable after the manifest is written. A
        // signed bundle therefore leaves this digest empty and relies on the
        // strict codesign gate; source-only packages retain the digest.
        || (!release.binary_sha256.is_empty()
            && release.binary_sha256 != sha256(&std::env::current_exe()?)?)
        || release.uv_sha256 != sha256(&uv)?
        || release.runtime_manifest_sha256
            != sha256(&crate::funasr::runtime_manifest_path(&runtime))?
    {
        return Err("release manifest does not match packaged inputs".into());
    }
    println!(
        "bi2read package is ready: {} · contract v{} · Runtime {} · uv {} · build {}",
        manifest.backend.label(),
        manifest.contract_version,
        release.runtime_tag,
        release.uv_version,
        release.build_number
    );
    Ok(())
}

fn sha256(path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(format!("cannot hash packaged file: {}", path.display()).into());
    }
    String::from_utf8(output.stdout)?
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "shasum returned no digest".into())
}
