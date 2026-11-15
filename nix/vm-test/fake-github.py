"""The GitHub API for one repository, serving HTTPS.

Commits are directories below the commits directory, named by their hash.
Branches are read from branches.json, which the test rewrites to push.
Issues are kept in issues.json for the test to inspect.
"""

import base64
import io
import json
import ssl
import sys
import tarfile
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

COMMITS, STATE, CERT, KEY = (Path(arg) for arg in sys.argv[1:5])
BRANCHES = STATE / "branches.json"
ISSUES = STATE / "issues.json"


def load(path: Path, default: Any) -> Any:
    return json.loads(path.read_text()) if path.exists() else default


def archive(sha: str) -> bytes:
    """The commit as GitHub's tarballs have it: below one directory."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
        tar.add(COMMITS / sha, arcname=f"owner-demo-{sha[:7]}")
    return buffer.getvalue()


class Handler(BaseHTTPRequestHandler):
    def respond(
        self, status: int, body: Any, content_type: str = "application/json"
    ) -> None:
        if content_type == "application/json":
            body = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def route(self) -> tuple[list[str], dict[str, str], Any]:
        url = urllib.parse.urlsplit(self.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        parts = url.path.strip("/").split("/")
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length)) if length else None
        return parts, query, body

    def do_GET(self) -> None:
        parts, query, _ = self.route()
        if parts[:3] != ["repos", "owner", "demo"]:
            return self.respond(404, {"message": "Not Found"})
        rest = parts[3:]
        first_page = query.get("page", "1") == "1"
        if rest == []:
            return self.respond(
                200, {"full_name": "owner/demo", "default_branch": "master"}
            )
        if rest == ["branches"]:
            branches: dict[str, str] = load(BRANCHES, {}) if first_page else {}
            return self.respond(
                200,
                [
                    {"name": name, "commit": {"sha": sha}}
                    for name, sha in branches.items()
                ],
            )
        if rest == ["pulls"]:
            return self.respond(200, [])
        if rest[:1] == ["contents"]:
            path = COMMITS / query.get("ref", "") / "/".join(rest[1:])
            if path.is_file():
                content = base64.b64encode(path.read_bytes()).decode()
                return self.respond(200, {"content": content})
            return self.respond(404, {"message": "Not Found"})
        if rest[:1] == ["tarball"] and (COMMITS / rest[1]).is_dir():
            return self.respond(200, archive(rest[1]), "application/x-gzip")
        if rest == ["issues"]:
            issues = [issue for issue in load(ISSUES, []) if issue["state"] == "open"]
            return self.respond(200, issues[::-1])
        return self.respond(404, {"message": "Not Found"})

    def do_POST(self) -> None:
        parts, _, body = self.route()
        issues = load(ISSUES, [])
        if parts == ["repos", "owner", "demo", "issues"]:
            number = len(issues) + 1
            issues.append(
                {
                    "number": number,
                    "title": body["title"],
                    "body": body["body"],
                    "state": "open",
                    "comments": [],
                }
            )
            ISSUES.write_text(json.dumps(issues))
            return self.respond(201, {"number": number})
        if parts[:4] == ["repos", "owner", "demo", "issues"] and parts[5:] == [
            "comments"
        ]:
            issues[int(parts[4]) - 1]["comments"].append(body["body"])
            ISSUES.write_text(json.dumps(issues))
            return self.respond(201, {})
        return self.respond(404, {"message": "Not Found"})

    def do_PATCH(self) -> None:
        parts, _, body = self.route()
        issues = load(ISSUES, [])
        if parts[:4] == ["repos", "owner", "demo", "issues"] and len(parts) == 5:
            issues[int(parts[4]) - 1].update(body)
            ISSUES.write_text(json.dumps(issues))
            return self.respond(200, {})
        return self.respond(404, {"message": "Not Found"})


server = ThreadingHTTPServer(("0.0.0.0", 443), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(CERT, KEY)
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
