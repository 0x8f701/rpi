#!/usr/bin/env python3
import argparse
import hashlib
import io
import os
import tarfile
from pathlib import Path

TARGETS = (
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--rpi", required=True)
    parser.add_argument("--license", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    binary = Path(args.rpi).read_bytes()
    license_bytes = Path(args.license).read_bytes()
    archives: list[Path] = []
    for target in TARGETS:
        archive = output / f"rpi-{args.version}-{target}.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            executable = tarfile.TarInfo("rpi")
            executable.size = len(binary)
            executable.mode = 0o755
            bundle.addfile(executable, io.BytesIO(binary))
            license_info = tarfile.TarInfo("LICENSE")
            license_info.size = len(license_bytes)
            license_info.mode = 0o644
            bundle.addfile(license_info, io.BytesIO(license_bytes))
        archives.append(archive)
    with (output / "SHA256SUMS").open("w", encoding="utf-8") as manifest:
        for archive in sorted(archives):
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            manifest.write(f"{digest}  {archive.name}\n")
    os.chmod(output / "SHA256SUMS", 0o644)


if __name__ == "__main__":
    main()
