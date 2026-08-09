#!/usr/bin/env python3
"""OpenAI 兼容 mock 上游（开发/冒烟测试用，非交付代码）。

用法: python3 scripts/mock_upstream.py   # 监听 0.0.0.0:9001
"""
import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = 9001


class Handler(BaseHTTPRequestHandler):
    def _json(self, status: int, obj: dict) -> None:
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/v1/models":
            self._json(200, {"object": "list", "data": [{"id": "mock-1", "object": "model"}]})
        else:
            self._json(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self._json(404, {"error": "not found"})
            return
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        model = body.get("model", "mock")
        usage = {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19}

        if body.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            pieces = ["Hello", " world", "!"]
            for i, piece in enumerate(pieces):
                chunk = {
                    "id": "chatcmpl-mock",
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {"content": piece},
                        "finish_reason": None if i < len(pieces) - 1 else "stop",
                    }],
                }
                if i == len(pieces) - 1:
                    chunk["usage"] = usage
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                self.wfile.flush()
                time.sleep(0.05)
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        else:
            self._json(200, {
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Hello world!"},
                    "finish_reason": "stop",
                }],
                "usage": usage,
            })

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    print(f"mock upstream listening on 0.0.0.0:{PORT}")
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
