#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
release_values=$(python3 - "$repo_root/packaging/release.toml" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as handle: data = tomllib.load(handle)
print(data["runtime_tag"]); print(data["uv_version"])
PY
)
runtime_tag=$(printf '%s\n' "$release_values" | sed -n '1p')
uv_version=$(printf '%s\n' "$release_values" | sed -n '2p')
build_root="${BIMYSCRIBE_BUILD_ROOT:-$repo_root/target/macos-local}"

case "$build_root" in /*) ;; *) printf 'BIMYSCRIBE_BUILD_ROOT must be absolute.\n' >&2; exit 2 ;; esac
for command_name in cargo codesign curl file git rg shasum tar; do
    command -v "$command_name" >/dev/null 2>&1 || {
        printf 'Required command is unavailable: %s\n' "$command_name" >&2
        exit 1
    }
done

runtime_repo="$build_root/runtime-$runtime_tag"
download_dir="$build_root/downloads"
tool_dir="$build_root/tools"
output_dir="$build_root/output"
archive_name="uv-aarch64-apple-darwin.tar.gz"
archive="$download_dir/$archive_name"
checksum="$archive.sha256"
uv_bin="$tool_dir/uv-aarch64-apple-darwin/uv"
app="$output_dir/BiMyScribe.app"

[ ! -e "$app" ] || {
    printf 'Refusing to replace an existing app: %s\n' "$app" >&2
    printf 'Choose a new BIMYSCRIBE_BUILD_ROOT or move the existing app.\n' >&2
    exit 1
}
mkdir -p "$download_dir" "$tool_dir" "$output_dir"

if [ ! -d "$runtime_repo/.git" ]; then
    git clone --branch "$runtime_tag" --depth 1 \
        https://github.com/xiaozhenliu/bimyscribe-funasr-runtime.git \
        "$runtime_repo"
fi
[ -z "$(git -C "$runtime_repo" status --porcelain --untracked-files=all)" ] || {
    printf 'Downloaded Runtime repository is dirty: %s\n' "$runtime_repo" >&2
    exit 1
}
git -C "$runtime_repo" rev-parse -q --verify "refs/tags/$runtime_tag" >/dev/null || {
    printf 'Downloaded Runtime does not contain tag %s.\n' "$runtime_tag" >&2
    exit 1
}

if [ ! -x "$uv_bin" ]; then
    base_url="https://github.com/astral-sh/uv/releases/download/$uv_version"
    curl --fail --location --output "$archive" "$base_url/$archive_name"
    curl --fail --location --output "$checksum" "$base_url/$archive_name.sha256"
    (cd "$download_dir" && shasum -a 256 -c "$(basename "$checksum")")
    tar -xzf "$archive" -C "$tool_dir"
fi

export CARGO_TARGET_DIR="$build_root/cargo-target"
"$repo_root/scripts/package-macos-app.sh" \
    --output-dir "$output_dir" \
    --runtime-repo "$runtime_repo" \
    --uv-bin "$uv_bin" \
    --private-revision "$(git -C "$repo_root" rev-parse HEAD)"

identity="${BIMYSCRIBE_SIGN_IDENTITY:-}"
if [ -z "$identity" ] && [ -t 0 ]; then
    printf '\nSigning identity (press Enter for free ad-hoc local signing):\n> '
    IFS= read -r identity
fi
identity=${identity:--}

sign_args=(--force --sign "$identity" --options runtime)
if [ "$identity" = "-" ]; then
    sign_args+=(--timestamp=none)
    printf 'Using ad-hoc signing for local use.\n'
else
    sign_args+=(--timestamp)
    printf 'Using signing identity: %s\n' "$identity"
fi

codesign "${sign_args[@]}" "$app/Contents/Resources/bin/uv"
codesign "${sign_args[@]}" "$app/Contents/MacOS/bimyscribe"
"$repo_root/scripts/finalize-release-manifest.sh" --signed "$app"
codesign "${sign_args[@]}" \
    --entitlements "$repo_root/packaging/macos/entitlements.plist" \
    "$app"
codesign --verify --deep --strict --verbose=2 "$app"
"$app/Contents/MacOS/bimyscribe" --package-self-check

printf '\nLocal macOS application is ready:\n%s\n' "$app"
if [ "$identity" = "-" ]; then
    printf 'This ad-hoc signed build is intended for this Mac, not public redistribution.\n'
else
    printf 'Developer ID signing succeeded; notarization is still required before public distribution.\n'
fi
