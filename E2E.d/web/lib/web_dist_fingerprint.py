#!/usr/bin/env python3
"""Stamp and verify Web bundles against the current production source tree."""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import shutil

import sys

STAMP_NAME = ".rpi-web-source.sha256"
STAMP_VERSION = "rpi-web-dist-v1"
SOURCE_FILES = (
    "index.html",
    "package-lock.json",
    "package.json",
    "scripts/trim-dist.mjs",
    "tsconfig.json",
    "vite.config.ts",
)
SOURCE_DIRS = ("public", "src")


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def source_paths(web_dir: Path) -> list[Path]:
    paths = [web_dir / relative for relative in SOURCE_FILES]
    for relative in SOURCE_DIRS:
        root = web_dir / relative
        if not root.is_dir():
            raise ValueError(f"missing Web source directory: {root}")
        paths.extend(path for path in root.rglob("*") if path.is_file())
    missing = [path for path in paths if not path.is_file()]
    if missing:
        raise ValueError(f"missing Web build input: {missing[0]}")
    symlinks = [path for path in paths if path.is_symlink()]
    if symlinks:
        raise ValueError(f"Web build input must not be a symlink: {symlinks[0]}")
    return sorted(paths, key=lambda path: path.relative_to(web_dir).as_posix())


def source_fingerprint(web_dir: Path) -> str:
    digest = hashlib.sha256()
    digest.update(f"{STAMP_VERSION}\0".encode())
    for path in source_paths(web_dir):
        relative = path.relative_to(web_dir).as_posix().encode()
        data = path.read_bytes()
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        digest.update(len(data).to_bytes(8, "big"))
        digest.update(data)
    return digest.hexdigest()


def stamp_values(web_dir: Path, dist_dir: Path) -> tuple[str, str]:
    index = dist_dir / "index.html"
    if not index.is_file() or index.is_symlink():
        raise ValueError(f"Web bundle index is missing or invalid: {index}")
    return source_fingerprint(web_dir), hash_file(index)


def write_stamp(web_dir: Path, dist_dir: Path) -> None:
    source_sha, index_sha = stamp_values(web_dir, dist_dir)
    stamp = dist_dir / STAMP_NAME
    stamp.write_text(
        f"version={STAMP_VERSION}\nsource_sha256={source_sha}\nindex_sha256={index_sha}\n",
        encoding="utf-8",
    )


def read_stamp(dist_dir: Path) -> dict[str, str]:
    stamp = dist_dir / STAMP_NAME
    if not stamp.is_file() or stamp.is_symlink():
        raise ValueError(f"Web bundle fingerprint is missing or invalid: {stamp}")
    values: dict[str, str] = {}
    for line in stamp.read_text(encoding="utf-8").splitlines():
        key, separator, value = line.partition("=")
        if not separator or not key or key in values:
            raise ValueError(f"malformed Web bundle fingerprint: {stamp}")
        values[key] = value
    expected_keys = {"version", "source_sha256", "index_sha256"}
    if set(values) != expected_keys or values["version"] != STAMP_VERSION:
        raise ValueError(f"unsupported Web bundle fingerprint: {stamp}")
    for key in ("source_sha256", "index_sha256"):
        value = values[key]
        if len(value) != 64 or any(char not in "0123456789abcdef" for char in value):
            raise ValueError(f"malformed {key} in Web bundle fingerprint: {stamp}")
    return values


def verify_stamp(web_dir: Path, dist_dir: Path) -> None:
    recorded = read_stamp(dist_dir)
    source_sha, index_sha = stamp_values(web_dir, dist_dir)
    if recorded["source_sha256"] != source_sha:
        raise ValueError(
            "Web bundle fingerprint does not match current sources; rebuild it from this tree"
        )
    if recorded["index_sha256"] != index_sha:
        raise ValueError("Web bundle index does not match its fingerprint")

def materialize(web_dir: Path, source_dist: Path, output_dist: Path) -> None:
    verify_stamp(web_dir, source_dist)
    if output_dist.exists():
        if output_dist.is_symlink() or not output_dist.is_dir():
            raise ValueError(f"Web bundle output is not a safe directory: {output_dist}")
        shutil.rmtree(output_dist)
    output_dist.mkdir(parents=True)
    (output_dist / "index.html").write_bytes((source_dist / "index.html").read_bytes())
    (output_dist / STAMP_NAME).write_bytes((source_dist / STAMP_NAME).read_bytes())



def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("source", "stamp", "verify", "materialize"))

    parser.add_argument("web_dir", type=Path)
    parser.add_argument("dist_dir", nargs="?", type=Path)
    parser.add_argument("output_dir", nargs="?", type=Path)

    args = parser.parse_args()

    web_dir = args.web_dir.resolve()
    try:
        if args.command == "source":
            if args.dist_dir is not None or args.output_dir is not None:
                parser.error("source does not accept dist_dir or output_dir")
            print(source_fingerprint(web_dir))
        else:
            if args.dist_dir is None:
                parser.error(f"{args.command} requires dist_dir")
            dist_dir = args.dist_dir.resolve()
            if args.command == "stamp":
                if args.output_dir is not None:
                    parser.error("stamp does not accept output_dir")
                write_stamp(web_dir, dist_dir)
            elif args.command == "verify":
                if args.output_dir is not None:
                    parser.error("verify does not accept output_dir")
                verify_stamp(web_dir, dist_dir)
            else:
                if args.output_dir is None:
                    parser.error("materialize requires output_dir")
                materialize(web_dir, dist_dir, args.output_dir.resolve())
    except (OSError, UnicodeError, ValueError) as error:
        print(f"web-dist fingerprint error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
