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
        if self.path == "/v1/responses":
            self._responses()
            return
        if self.path != "/v1/chat/completions":
            self._json(404, {"error": "not found"})
            return
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        model = body.get("model", "mock")
        usage = {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19}

        # 异常场景：200 + JSON 错误体（非 SSE）——验证网关不再吞掉错误
        if json.dumps(body).find("boom") != -1:
            self._json(200, {"error": {"code": "mock_error", "message": "boom requested"}})
            return

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

    def _responses(self):
        """原生 /v1/responses（透传冒烟用）：非流式返回 Responses 响应；流式发 Responses SSE"""
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        model = body.get("model", "mock")
        usage = {"input_tokens": 12, "output_tokens": 7, "total_tokens": 19}

        if body.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for i, piece in enumerate(["Hello", " world", "!"]):
                evt = {
                    "type": "response.output_text.delta",
                    "delta": piece,
                    "output_index": 0,
                    "content_index": 0,
                    "item_id": "resp_mock_msg_0",
                }
                self.wfile.write(f"event: {evt['type']}\ndata: {json.dumps(evt)}\n\n".encode())
                self.wfile.flush()
                time.sleep(0.05)
            completed = {
                "type": "response.completed",
                "response": {
                    "id": "resp_mock",
                    "object": "response",
                    "status": "completed",
                    "model": model,
                    "output": [{
                        "type": "message",
                        "id": "resp_mock_msg_0",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "Hello world!", "annotations": []}],
                    }],
                    "usage": usage,
                },
            }
            self.wfile.write(f"event: response.completed\ndata: {json.dumps(completed)}\n\n".encode())
            self.wfile.write(b"event: response.done\ndata: {\"type\": \"response.done\"}\n\n")
            self.wfile.flush()
        else:
            self._json(200, {
                "id": "resp_mock",
                "object": "response",
                "status": "completed",
                "model": model,
                "output": [{
                    "type": "message",
                    "id": "resp_mock_msg_0",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Hello world!", "annotations": []}],
                }],
                "usage": usage,
            })

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    print(f"mock upstream listening on 0.0.0.0:{PORT}")
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
