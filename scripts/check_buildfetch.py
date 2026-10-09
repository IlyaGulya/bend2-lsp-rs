#!/usr/bin/env python3
"""Prove WebDAV writes and cross-server readonly hits using a real Rust library."""

import argparse
import base64
import json
import os
from pathlib import Path
import subprocess
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET


def run(command, **kwargs):
    return subprocess.run(command, check=True, capture_output=True, text=True, **kwargs)


def quota():
    credentials = f"{os.environ['SCCACHE_WEBDAV_USERNAME']}:{os.environ['SCCACHE_WEBDAV_PASSWORD']}"
    authorization = base64.b64encode(credentials.encode()).decode()
    request = urllib.request.Request(
        os.environ["SCCACHE_WEBDAV_ENDPOINT"],
        data=b'<d:propfind xmlns:d="DAV:"><d:prop><d:quota-used-bytes/><d:quota-available-bytes/></d:prop></d:propfind>',
        headers={"Authorization": f"Basic {authorization}", "Depth": "0", "Content-Type": "application/xml"},
        method="PROPFIND",
    )
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            root = ET.fromstring(response.read())
            used = root.findtext(".//{DAV:}quota-used-bytes")
            available = root.findtext(".//{DAV:}quota-available-bytes")
            return {
                "used_bytes": int(used) if used else None,
                "available_bytes": int(available) if available else None,
                "http_status": response.status,
            }
    except urllib.error.HTTPError as error:
        return {"used_bytes": None, "available_bytes": None, "http_status": error.code}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("write", "read"))
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--report", required=True, type=Path)
    args = parser.parse_args()
    directory = args.directory.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    source = directory / "probe.rs"
    expected = int(os.environ["GITHUB_RUN_ID"])
    if args.mode == "write":
        source.write_text(f"pub fn answer() -> u64 {{ {expected} }}\n")
    output = directory / args.mode
    output.mkdir(exist_ok=True)
    log_path = directory / f"sccache-{args.mode}.log"
    os.environ["SCCACHE_LOG"] = "info"
    os.environ["SCCACHE_ERROR_LOG"] = str(log_path)
    run(["sccache", "--zero-stats"])
    environment = os.environ.copy()
    if args.mode == "write":
        environment["SCCACHE_RECACHE"] = "1"
    run([
        "sccache", "rustc", "--crate-type=rlib", "--crate-name=buildfetch_probe",
        "--edition=2024", "--emit=link", str(source), "--out-dir", str(output),
    ], env=environment)
    info = json.loads(run(["sccache", "--show-stats", "--stats-format=json"]).stdout)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(info, indent=2) + "\n")
    if "webdav" not in info["cache_location"].lower():
        raise RuntimeError("compiler probe did not use the WebDAV backend")
    stats = info["stats"]
    if stats["cache_write_errors"] or stats["cache_read_errors"]:
        if log_path.exists():
            diagnostic = log_path.read_text()
            password = os.environ["SCCACHE_WEBDAV_PASSWORD"]
            credentials = f"{os.environ['SCCACHE_WEBDAV_USERNAME']}:{password}"
            for secret in (base64.b64encode(credentials.encode()).decode(), credentials, password):
                diagnostic = diagnostic.replace(secret, "[REDACTED]")
            args.report.with_suffix(".error.txt").write_text(diagnostic)
            print(diagnostic)
        raise RuntimeError("remote compiler cache reported I/O errors")
    if args.mode == "write":
        if not stats["cache_writes"]:
            raise RuntimeError("read-write token did not store the compiler artifact")
    elif not stats["cache_hits"]["counts"].get("Rust", 0) or stats["cache_writes"]:
        raise RuntimeError("readonly token did not reuse the remote compiler artifact")
    if args.mode == "read":
        library = output / "libbuildfetch_probe.rlib"
        if library.read_bytes() != (directory / "write/libbuildfetch_probe.rlib").read_bytes():
            raise RuntimeError("cached compiler artifact differs from the written artifact")
        consumer = directory / "consumer.rs"
        consumer.write_text(f"fn main() {{ assert_eq!(buildfetch_probe::answer(), {expected}); }}\n")
        executable = directory / "consumer"
        run(["rustc", str(consumer), "--extern", f"buildfetch_probe={library}", "-o", str(executable)])
        run([str(executable)])
        info["webdav_quota"] = quota()
        args.report.write_text(json.dumps(info, indent=2) + "\n")
    print(f"BuildFetch {args.mode} probe passed")


if __name__ == "__main__":
    main()
