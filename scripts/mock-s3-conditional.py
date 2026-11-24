#!/usr/bin/env python3
"""本地最小 S3 服务：验证 RClaw 的条件创建（If-None-Match: *）客户端行为。

只实现本测试用到的路径式请求：PUT/GET/DELETE /<bucket>/<key>。
MOCK_IGNORE=1 时故意忽略 If-None-Match（模拟不支持条件写的服务端），用来验证
RClaw 的验收逻辑会把这种情况判为失败。
"""
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse, unquote

PORT = int(os.environ.get("MOCK_PORT", "19000"))
IGNORE = os.environ.get("MOCK_IGNORE") == "1"
STORE = {}
LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("mock-s3: " + (fmt % args) + "\n")

    def _bucket_key(self):
        path = unquote(urlparse(self.path).path)
        parts = path.lstrip("/").split("/", 1)
        bucket = parts[0]
        key = parts[1] if len(parts) > 1 else ""
        return bucket, key

    def _send(self, code, body=b"", headers=None):
        self.send_response(code)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_PUT(self):
        bucket, key = self._bucket_key()
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        cond = self.headers.get("If-None-Match")
        with LOCK:
            exists = key in STORE
            if cond == "*" and exists and not IGNORE:
                n = self.headers.get("If-None-Match")
                self.log_message("PUT %s if-none-match=%s -> 412 (exists)", key, n)
                self._send(412, b"<Error><Code>PreconditionFailed</Code></Error>")
                return
            STORE[key] = body
        self.log_message("PUT %s if-none-match=%s -> 200 (%d bytes)", key, cond, len(body))
        self._send(200, headers={"ETag": '"mock-etag"'})

    def do_GET(self):
        bucket, key = self._bucket_key()
        query = urlparse(self.path).query
        if "list-type=2" in query:
            prefix = ""
            for part in query.split("&"):
                if part.startswith("prefix="):
                    prefix = unquote(part.split("=", 1)[1])
            with LOCK:
                items = sorted(k for k in STORE if k.startswith(prefix))
            body = ['<?xml version="1.0" encoding="UTF-8"?>', "<ListBucketResult>"]
            body.append("<Name>%s</Name><KeyCount>%d</KeyCount>" % (bucket, len(items)))
            body.append("<IsTruncated>false</IsTruncated>")
            for k in items:
                body.append(
                    "<Contents><Key>%s</Key><Size>%d</Size><ETag>\"mock-etag\"</ETag></Contents>"
                    % (k, len(STORE[k]))
                )
            body.append("</ListBucketResult>")
            self._send(200, "".join(body).encode())
            return
        with LOCK:
            body = STORE.get(key)
        if body is None:
            self.log_message("GET %s -> 404", key)
            self._send(404, b"<Error><Code>NoSuchKey</Code></Error>")
            return
        self.log_message("GET %s -> 200 (%d bytes)", key, len(body))
        self._send(200, body)

    def do_DELETE(self):
        bucket, key = self._bucket_key()
        with LOCK:
            STORE.pop(key, None)
        self.log_message("DELETE %s -> 204", key)
        self._send(204)


if __name__ == "__main__":
    server = HTTPServer(("127.0.0.1", PORT), Handler)
    print("mock-s3 listening on 127.0.0.1:%d ignore_conditional=%s" % (PORT, IGNORE), flush=True)
    server.serve_forever()
