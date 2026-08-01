#!/usr/bin/env python3
import argparse
import json
import os
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *args, directory: str, version: str, assets: list[str], **kwargs):
        self.version = version
        self.assets = assets
        super().__init__(*args, directory=directory, **kwargs)

    def do_GET(self) -> None:
        if self.path in {"/releases/latest", f"/releases/tags/v{self.version}"}:
            host, port = self.server.server_address
            base = f"http://{host}:{port}"
            body = json.dumps(
                {
                    "tag_name": f"v{self.version}",
                    "name": f"rpi v{self.version}",
                    "draft": False,
                    "prerelease": "-" in self.version,
                    "html_url": f"{base}/release/v{self.version}",
                    "body": "local deterministic fixture",
                    "assets": [
                        {
                            "name": name,
                            "browser_download_url": f"{base}/{name}",
                            "size": (Path(self.directory) / name).stat().st_size,
                        }
                        for name in self.assets
                    ],
                }
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        super().do_GET()

    def log_message(self, fmt: str, *args: object) -> None:
        print(fmt % args, flush=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--port-file", required=True)
    parser.add_argument("assets", nargs="+")
    args = parser.parse_args()
    root = str(Path(args.root).resolve())
    server = ThreadingHTTPServer(
        ("127.0.0.1", 0),
        lambda *a, **kw: Handler(*a, directory=root, version=args.version, assets=args.assets, **kw),
    )
    Path(args.port_file).write_text(str(server.server_address[1]), encoding="utf-8")
    os.chmod(args.port_file, 0o600)
    server.serve_forever()


if __name__ == "__main__":
    main()
