#!/usr/bin/env python3
"""Generate and verify deterministic BiMyScribe public snapshot manifests."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import sys
import tomllib
from pathlib import Path


MANIFEST_NAME = "publication-manifest.json"
POLICY_PATH = "packaging/public-paths.txt"
RELEASE_CONFIG_PATH = "packaging/release.toml"
PUBLIC_OWNED_PREFIXES = (".git/", ".github/")
PUBLIC_OWNED_FILES = {".gitignore", "SECURITY.md", "CONTRIBUTING.md"}
MANAGED_WORKFLOWS = {
    ".github/workflows/public-snapshot.yml",
    ".github/workflows/release-source.yml",
}
FORBIDDEN_PREFIXES = (
    ".agents/",
    ".scratch/",
    "docs/agents/",
    "docs/archive/",
    "docs/design/plans/",
)
FORBIDDEN_FILES = {
    ".publicignore",
    "AGENTS.md",
    "CLAUDE.md",
    "docs/README.md",
    "docs/macos-code-signing.md",
    "docs/macos-release-guide.md",
    "docs/product-requirements.md",
    "docs/public-release-runbook.md",
    "docs/publication-ci-rfc.md",
    "docs/publication-exclusions.md",
    "docs/publication-model.md",
    "scripts/check-governance.sh",
    "scripts/publish-current.sh",
    "scripts/run-native-uv-validation.sh",
}
SCANNER_PATH = "scripts/check-public-snapshot.py"
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
REVISION_RE = re.compile(r"^[0-9a-f]{40}$")
PRIVATE_KEY_RE = re.compile(rb"-----BEGIN(?: [A-Z]+)? PRIVATE KEY-----")
CREDENTIAL_RE = re.compile(
    rb"(?:AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9_]{30,}|"
    rb"sk-[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{20,})"
)
PRIVATE_REFERENCE_RE = re.compile(
    r"(?:/Users/xz(?:/|\b)|ORICOFiles|docs/(?:agents|archive)/|"
    r"docs/product-requirements\.md|docs/publication-(?:model|exclusions|ci-rfc)\.md|"
    r"docs/public-release-runbook\.md|(?:^|/)AGENTS\.md|(?:^|/)CLAUDE\.md|\.scratch/)"
)


class SnapshotError(RuntimeError):
    pass


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def relative(path: Path, root: Path) -> str:
    return path.relative_to(root).as_posix()


def is_public_owned(path: str) -> bool:
    if path in MANAGED_WORKFLOWS:
        return False
    return path in PUBLIC_OWNED_FILES or path.startswith(PUBLIC_OWNED_PREFIXES)


def load_policy(root: Path) -> list[str]:
    policy_file = root / POLICY_PATH
    if not policy_file.is_file():
        raise SnapshotError(f"missing public path policy: {POLICY_PATH}")
    entries: list[str] = []
    for number, raw in enumerate(policy_file.read_text(encoding="utf-8").splitlines(), 1):
        entry = raw.strip()
        if not entry or entry.startswith("#"):
            continue
        if entry.startswith("/") or ".." in Path(entry).parts or entry == MANIFEST_NAME:
            raise SnapshotError(f"invalid public path policy entry at line {number}: {entry}")
        if entry in entries:
            raise SnapshotError(f"duplicate public path policy entry: {entry}")
        entries.append(entry)
    if POLICY_PATH not in entries:
        raise SnapshotError(f"public path policy must include itself: {POLICY_PATH}")
    missing_workflows = sorted(MANAGED_WORKFLOWS.difference(entries))
    if missing_workflows:
        raise SnapshotError(
            "public path policy must include managed workflows: "
            + ", ".join(missing_workflows)
        )
    return entries


def is_allowed(path: str, policy: list[str]) -> bool:
    if path == MANIFEST_NAME:
        return True
    for entry in policy:
        if entry.endswith("/"):
            if path.startswith(entry):
                return True
        elif path == entry:
            return True
    return False


def synchronized_files(root: Path) -> list[Path]:
    files: list[Path] = []
    for path in root.rglob("*"):
        rel = relative(path, root)
        if is_public_owned(rel) or rel == MANIFEST_NAME:
            continue
        if path.is_symlink():
            raise SnapshotError(f"public snapshot may not contain symlinks: {rel}")
        if path.is_file():
            files.append(path)
    return sorted(files, key=lambda item: relative(item, root).encode("utf-8"))


def check_paths(root: Path, policy: list[str], files: list[Path]) -> None:
    errors: list[str] = []
    for path in files:
        rel = relative(path, root)
        if rel in FORBIDDEN_FILES or rel.startswith(FORBIDDEN_PREFIXES):
            errors.append(f"forbidden private path: {rel}")
        if not is_allowed(rel, policy):
            errors.append(f"path is not present in public allowlist: {rel}")
    if errors:
        raise SnapshotError("\n".join(errors))


def check_contents(root: Path, files: list[Path]) -> None:
    errors: list[str] = []
    for path in files:
        rel = relative(path, root)
        data = path.read_bytes()
        if rel != SCANNER_PATH:
            if PRIVATE_KEY_RE.search(data):
                errors.append(f"private-key material detected: {rel}")
            if CREDENTIAL_RE.search(data):
                errors.append(f"credential-like material detected: {rel}")
        if b"\0" in data:
            continue
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            continue
        if rel != SCANNER_PATH and PRIVATE_REFERENCE_RE.search(text):
            errors.append(f"private development reference detected: {rel}")
    if errors:
        raise SnapshotError("\n".join(errors))


def package_version(root: Path) -> str:
    with (root / "Cargo.toml").open("rb") as handle:
        value = tomllib.load(handle).get("package", {}).get("version")
    if not isinstance(value, str) or not value:
        raise SnapshotError("Cargo.toml package.version is missing")
    return value


def release_config(root: Path) -> dict[str, object]:
    with (root / RELEASE_CONFIG_PATH).open("rb") as handle:
        data = tomllib.load(handle)
    required = ("distribution_mode", "runtime_tag", "runtime_revision", "release_notes_path")
    missing = [key for key in required if not data.get(key)]
    if missing:
        raise SnapshotError(f"release config keys are missing: {', '.join(missing)}")
    return data


def check_release_inputs(root: Path) -> tuple[str, dict[str, object]]:
    version = package_version(root)
    config = release_config(root)
    if config["distribution_mode"] != "source-only":
        raise SnapshotError("one-command publication currently requires distribution_mode=source-only")
    notes_path = f"docs/releases/v{version}.md"
    if config["release_notes_path"] != notes_path:
        raise SnapshotError(
            f"release_notes_path must be {notes_path}, got {config['release_notes_path']}"
        )
    notes = root / notes_path
    if not notes.is_file():
        raise SnapshotError(f"missing versioned release notes: {notes_path}")
    compatibility_notes = root / "RELEASE_NOTES.md"
    if compatibility_notes.read_bytes() != notes.read_bytes():
        raise SnapshotError(f"RELEASE_NOTES.md must be byte-identical to {notes_path}")
    changelog = (root / "CHANGELOG.md").read_text(encoding="utf-8")
    if f"## {version}" not in changelog:
        raise SnapshotError(f"CHANGELOG.md has no {version} entry")
    with (root / "Cargo.lock").open("rb") as handle:
        lock = tomllib.load(handle)
    lock_versions = [
        item.get("version") for item in lock.get("package", []) if item.get("name") == "bimyscribe"
    ]
    if lock_versions != [version]:
        raise SnapshotError(f"Cargo.lock bimyscribe version mismatch: {lock_versions!r}")
    return version, config


def file_entries(root: Path, files: list[Path]) -> list[dict[str, object]]:
    result: list[dict[str, object]] = []
    for path in files:
        mode = path.stat().st_mode
        result.append(
            {
                "executable": bool(mode & stat.S_IXUSR),
                "path": relative(path, root),
                "sha256": sha256_file(path),
                "size": path.stat().st_size,
            }
        )
    return result


def validate_root(root: Path) -> tuple[list[str], list[Path], str, dict[str, object]]:
    policy = load_policy(root)
    files = synchronized_files(root)
    check_paths(root, policy, files)
    check_contents(root, files)
    synchronized = {relative(path, root) for path in files}
    missing_workflows = sorted(MANAGED_WORKFLOWS.difference(synchronized))
    if missing_workflows:
        raise SnapshotError(
            "managed public workflows are missing: " + ", ".join(missing_workflows)
        )
    version, config = check_release_inputs(root)
    return policy, files, version, config


def generate(args: argparse.Namespace) -> None:
    root = args.root.resolve()
    if not REVISION_RE.fullmatch(args.private_revision):
        raise SnapshotError("private revision must be a full 40-character lowercase SHA")
    policy, files, version, config = validate_root(root)
    ignore_path = args.ignore_file.resolve()
    if not ignore_path.is_file():
        raise SnapshotError(f"missing private ignore file: {ignore_path}")
    manifest = {
        "app_version": version,
        "files": file_entries(root, files),
        "private_revision": args.private_revision,
        "public_path_policy_sha256": sha256_file(root / POLICY_PATH),
        "publicignore_sha256": sha256_file(ignore_path),
        "release_config_sha256": sha256_file(root / RELEASE_CONFIG_PATH),
        "release_requested": True,
        "runtime_revision": config["runtime_revision"],
        "runtime_tag": config["runtime_tag"],
        "schema_version": 1,
    }
    output = root / MANIFEST_NAME
    output.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(f"generated {output} for {len(files)} synchronized files")


def verify(args: argparse.Namespace) -> None:
    root = args.root.resolve()
    manifest_path = root / MANIFEST_NAME
    if not manifest_path.is_file():
        raise SnapshotError(f"missing {MANIFEST_NAME}")
    with manifest_path.open(encoding="utf-8") as handle:
        manifest = json.load(handle)
    policy, files, version, config = validate_root(root)
    expected = {
        "app_version": version,
        "files": file_entries(root, files),
        "private_revision": manifest.get("private_revision"),
        "public_path_policy_sha256": sha256_file(root / POLICY_PATH),
        "publicignore_sha256": manifest.get("publicignore_sha256"),
        "release_config_sha256": sha256_file(root / RELEASE_CONFIG_PATH),
        "release_requested": True,
        "runtime_revision": config["runtime_revision"],
        "runtime_tag": config["runtime_tag"],
        "schema_version": 1,
    }
    if not REVISION_RE.fullmatch(str(manifest.get("private_revision", ""))):
        raise SnapshotError("manifest private_revision is not a full lowercase SHA")
    if not SHA256_RE.fullmatch(str(manifest.get("publicignore_sha256", ""))):
        raise SnapshotError("manifest publicignore_sha256 is invalid")
    if args.expected_private_revision and manifest.get("private_revision") != args.expected_private_revision:
        raise SnapshotError(
            "manifest private_revision does not match expected revision: "
            f"{manifest.get('private_revision')} != {args.expected_private_revision}"
        )
    if manifest != expected:
        expected_text = json.dumps(expected, ensure_ascii=False, indent=2, sort_keys=True)
        actual_text = json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True)
        raise SnapshotError(f"manifest mismatch\nexpected:\n{expected_text}\nactual:\n{actual_text}")
    print(f"verified {MANIFEST_NAME} for {len(files)} synchronized files")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    subparsers = result.add_subparsers(dest="command", required=True)
    generate_parser = subparsers.add_parser("generate")
    generate_parser.add_argument("--root", type=Path, required=True)
    generate_parser.add_argument("--private-revision", required=True)
    generate_parser.add_argument("--ignore-file", type=Path, required=True)
    generate_parser.set_defaults(handler=generate)
    verify_parser = subparsers.add_parser("verify")
    verify_parser.add_argument("--root", type=Path, required=True)
    verify_parser.add_argument("--expected-private-revision")
    verify_parser.set_defaults(handler=verify)
    return result


def main() -> int:
    args = parser().parse_args()
    try:
        args.handler(args)
    except (OSError, ValueError, json.JSONDecodeError, SnapshotError) as error:
        print(f"public snapshot check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
