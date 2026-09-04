#!/usr/bin/env python3
"""OpenAI 兼容 mock 上游（开发/冒烟测试用，非交付代码）。

用法: python3 scripts/mock_upstream.py   # 监听 0.0.0.0:9001
"""
import gzip as _gzip
import json
import os
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
        if self.path in ("/1/status", "/v1/1/status"):
            self._status()
            return
        if self.path == "/v1/models":
            self._json(200, {"object": "list", "data": [{"id": "mock-1", "object": "model"}]})
        else:
            self._json(404, {"error": "not found"})

    def _status(self):
        """/1/status 与 /v1/1/status 健康探测端点：模式控制返回形态（状态页冒烟用）。
        ok(默认)/degraded/down/html/shapeless/404/500/timeout。
        模式来源：/tmp/mock_status_mode 文件 > MOCK_STATUS_MODE 环境变量（免重启切换）"""
        try:
            with open("/tmp/mock_status_mode") as f:
                mode = f.read().strip()
        except OSError:
            mode = os.environ.get("MOCK_STATUS_MODE", "ok")
        if mode == "timeout":
            time.sleep(120)  # 探测方超时先行，本响应永不返回
            return
        if mode == "404":
            self._json(404, {"error": "not found"})
            return
        if mode == "500":
            self._json(500, {"error": "boom"})
            return
        if mode == "html":
            body = b"<html><body>login page</body></html>"
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if mode == "shapeless":
            self._json(200, {"foo": 1})
            return
        if mode == "degraded":
            self._json(200, {"status": "degraded", "service": "mock-upstream"})
            return
        if mode == "down":
            self._json(200, {"status": "down"})
            return
        self._json(200, {"status": "ok", "service": "mock-upstream", "version": "1.0.0"})

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
        try:
            with open("/tmp/mock_last_request.json", "w") as f:
                json.dump({"path": self.path, "body": body}, f, ensure_ascii=False, indent=1)
        except OSError:
            pass
        usage = {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19}
        try:
            with open("/tmp/mock_mode.txt") as f:
                mode = f.read().strip()
        except OSError:
            mode = ""

        if mode == "json_body_for_stream" and body.get("stream"):
            # 流式请求但回完整 Chat completion JSON（上游忽略 stream 参数）
            self._json(200, {
                "id": "chatcmpl-mock", "object": "chat.completion", "model": model,
                "choices": [{"index": 0,
                             "message": {"role": "assistant", "content": "Full JSON reply!"},
                             "finish_reason": "stop"}],
                "usage": usage})
            return
        # ---- quirk 模式（复现上游不守协议形态）----
        if mode == "content_parts_nonstream" and not body.get("stream"):
            # 非流式：message.content 为 parts 数组（type=text）
            self._json(200, {
                "id": "chatcmpl-mock", "object": "chat.completion", "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": [
                    {"type": "text", "text": "Hello "}, {"type": "text", "text": "world!"}]},
                    "finish_reason": "stop"}],
                "usage": usage})
            return
        if mode == "content_parts_stream" and body.get("stream"):
            # 流式：delta.content 为 parts 数组（Gemini 兼容层等）
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            pieces = ["Hello", " world", "!"]
            for i, piece in enumerate(pieces):
                chunk = {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
                         "choices": [{"index": 0,
                                      "delta": {"content": [{"type": "text", "text": piece}]},
                                      "finish_reason": None if i < len(pieces) - 1 else "stop"}]}
                if i == len(pieces) - 1:
                    chunk["usage"] = usage
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                self.wfile.flush()
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        if mode == "string_usage_stream" and body.get("stream"):
            # 流式：usage 值为字符串（部分网关）；首帧也带字符串 role
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            chunk = {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
                     "choices": [{"index": 0, "delta": {"content": "Hello world!"}, "finish_reason": "stop"}],
                     "usage": {"prompt_tokens": "12", "completion_tokens": "7", "total_tokens": "19"}}
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        if mode == "crlf_stream" and body.get("stream"):
            # 流式：CRLF 行结尾的 SSE
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            chunk = {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
                     "choices": [{"index": 0, "delta": {"content": "Hello world!"}, "finish_reason": "stop"}]}
            self.wfile.write(f"data: {json.dumps(chunk)}\r\n\r\n".encode())
            self.wfile.write(b"data: [DONE]\r\n\r\n")
            self.wfile.flush()
            return
        if mode == "reasoning_object_stream" and body.get("stream"):
            # 流式：delta.reasoning 为对象（部分推理网关）
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            frames = [
                {"id": "c1", "object": "chat.completion.chunk", "model": model,
                 "choices": [{"index": 0, "delta": {"reasoning": {"text": "thinking..."}}, "finish_reason": None}]},
                {"id": "c1", "object": "chat.completion.chunk", "model": model,
                 "choices": [{"index": 0, "delta": {"content": "Hello world!"}, "finish_reason": "stop"}]},
            ]
            for fr in frames:
                self.wfile.write(f"data: {json.dumps(fr)}\n\n".encode())
                self.wfile.flush()
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        if mode == "reasoning_first_stream":
            # DeepSeek 推理模型形态：reasoning_content 先于正文输出
            if body.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                frames = [
                    {"id": "c1", "object": "chat.completion.chunk", "model": model,
                     "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]},
                    {"id": "c1", "object": "chat.completion.chunk", "model": model,
                     "choices": [{"index": 0, "delta": {"reasoning_content": "Let me "}, "finish_reason": None}]},
                    {"id": "c1", "object": "chat.completion.chunk", "model": model,
                     "choices": [{"index": 0, "delta": {"reasoning_content": "think."}, "finish_reason": None}]},
                    {"id": "c1", "object": "chat.completion.chunk", "model": model,
                     "choices": [{"index": 0, "delta": {"content": "Hello world!"}, "finish_reason": "stop"}],
                     "usage": usage},
                ]
                for fr in frames:
                    self.wfile.write(f"data: {json.dumps(fr)}\n\n".encode())
                    self.wfile.flush()
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                return
            self._json(200, {
                "id": "chatcmpl-mock", "object": "chat.completion", "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello world!",
                                                     "reasoning_content": "Let me think."},
                             "finish_reason": "stop"}],
                "usage": usage})
            return
        if mode == "length_stream":
            # 截断：finish_reason=length（Responses → incomplete）
            if body.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                frames = [
                    {"id": "c1", "object": "chat.completion.chunk", "model": model,
                     "choices": [{"index": 0, "delta": {"content": "partial out"}, "finish_reason": None}]},
                    {"id": "c1", "object": "chat.completion.chunk", "model": model,
                     "choices": [{"index": 0, "finish_reason": "length"}],
                     "usage": usage},
                ]
                for fr in frames:
                    self.wfile.write(f"data: {json.dumps(fr)}\n\n".encode())
                    self.wfile.flush()
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                return
            self._json(200, {
                "id": "chatcmpl-mock", "object": "chat.completion", "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "partial out"},
                             "finish_reason": "length"}],
                "usage": usage})
            return
        if mode == "unnamed_tool_stream" and body.get("stream"):
            # 工具帧始终缺函数名（#4341 护栏：不得伪装成空成功）
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            frames = [
                {"id": "c1", "object": "chat.completion.chunk", "model": model,
                 "choices": [{"index": 0, "delta": {"tool_calls": [
                     {"index": 0, "id": "call_x", "function": {"arguments": "{\"a\":"}}]}, "finish_reason": None}]},
                {"id": "c1", "object": "chat.completion.chunk", "model": model,
                 "choices": [{"index": 0, "delta": {"tool_calls": [
                     {"index": 0, "function": {"arguments": "1}"}}]}, "finish_reason": "tool_calls"}],
                 "usage": usage},
            ]
            for fr in frames:
                self.wfile.write(f"data: {json.dumps(fr)}\n\n".encode())
                self.wfile.flush()
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        if mode == "cache_usage_stream" and body.get("stream"):
            # usage 带 prompt_tokens_details.cached_tokens（Anthropic 三桶恒等式）
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            frames = [
                {"id": "c1", "object": "chat.completion.chunk", "model": model,
                 "choices": [{"index": 0, "delta": {"content": "cached reply"}, "finish_reason": None}]},
                {"id": "c1", "object": "chat.completion.chunk", "model": model,
                 "choices": [{"index": 0, "finish_reason": "stop"}],
                 "usage": {"prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
                           "prompt_tokens_details": {"cached_tokens": 40}}},
            ]
            for fr in frames:
                self.wfile.write(f"data: {json.dumps(fr)}\n\n".encode())
                self.wfile.flush()
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        if mode == "no_index_tool_stream" and body.get("stream"):
            # H4 回归：工具帧缺 index（并行工具帧 key 解析）
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            frames = [
                {"id": "c1", "object": "chat.completion.chunk", "model": model,
                 "choices": [{"index": 0, "delta": {"tool_calls": [
                     {"id": "call_nx", "type": "function",
                      "function": {"name": "get_weather", "arguments": ""}}]}, "finish_reason": None}]},
                {"id": "c1", "object": "chat.completion.chunk", "model": model,
                 "choices": [{"index": 0, "delta": {"tool_calls": [
                     {"function": {"arguments": "{\"city\":\"Paris\"}"}}]},
                     "finish_reason": "tool_calls"}],
                 "usage": usage},
            ]
            for fr in frames:
                self.wfile.write(f"data: {json.dumps(fr)}\n\n".encode())
                self.wfile.flush()
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        if mode == "error_first_chunk_stream" and body.get("stream"):
            # M1 回归：200 + 首帧即错误 envelope（网关应拒绝/换家而非透传垃圾帧）
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(b'data: {"error": {"message": "boom envelope", "type": "invalid_request_error"}}\n\n')
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        if mode == "error_second_chunk_stream" and body.get("stream"):
            # P0-2 回归：200 + 首块为 role delta（无产出）+ 次块即错误 envelope。
            # priming 窗口必须把错误块纳入换家判定——客户端不得收到 failed 流
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(b'data: {"id":"c1","object":"chat.completion.chunk","model":"mock","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}\n\n')
            self.wfile.write(b'data: {"error": {"message": "boom on second chunk", "type": "invalid_request_error"}}\n\n')
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        if mode == "rectify_image_trigger":
            # H6 反应式整流回归：含 image_url → 400 仅文本；整流后重试体无图 → 正常响应
            if "image_url" in json.dumps(body):
                self._json(400, {"error": {"code": "unsupported_media",
                                           "message": "Model only support text input"}})
                return
            # 无图片（整流重试体）→ 落入默认响应
        if mode == "gzip_response" and not body.get("stream"):
            # 上游响应 Content-Encoding: gzip（reqwest 自动解压回归）
            payload = json.dumps({
                "id": "chatcmpl-mock", "object": "chat.completion", "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant",
                                                     "content": "gzip decoded reply"},
                             "finish_reason": "stop"}],
                "usage": usage,
            }).encode()
            body_enc = _gzip.compress(payload)
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Encoding", "gzip")
            self.send_header("Content-Length", str(len(body_enc)))
            self.end_headers()
            self.wfile.write(body_enc)
            return
        usage = {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19}
        # 异常场景：200 + JSON 错误体（非 SSE）——验证网关不再吞掉错误
        if json.dumps(body).find("boom") != -1:
            self._json(200, {"error": {"code": "mock_error", "message": "boom requested"}})
            return

        # 工具场景：请求带 tools → 返回 tool_calls（finish_reason=tool_calls）；
 # 带 reasoning_content 模拟 DeepSeek 思考输出
        has_tools = bool(body.get("tools"))
        wants_reasoning = any(
            m.get("reasoning_content") for m in body.get("messages", [])
        ) or "think" in json.dumps(body)

        if body.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            pieces = [] if has_tools else ["Hello", " world", "!"]
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
            if wants_reasoning:
                for piece in ["Let me ", "think."]:
                    chunk = {
                        "id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
                        "choices": [{"index": 0,
                                     "delta": {"reasoning_content": piece},
                                     "finish_reason": None}],
                    }
                    self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                    self.wfile.flush()
                    time.sleep(0.05)
            if has_tools:
                tool_frames = [
                    {"index": 0, "id": "call_mock_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": ""}},
                    {"index": 0, "function": {"arguments": "{\"city\":\"Paris\"}"}},
                ]
                for i, tf in enumerate(tool_frames):
                    last = i == len(tool_frames) - 1
                    chunk = {
                        "id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model,
                        "choices": [{"index": 0, "delta": {"tool_calls": [tf]},
                                     "finish_reason": "tool_calls" if last else None}],
                    }
                    if last:
                        chunk["usage"] = usage
                    self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                    self.wfile.flush()
                    time.sleep(0.05)
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        else:
            message = {"role": "assistant", "content": "Hello world!"}
            finish = "stop"
            if wants_reasoning:
                message["reasoning_content"] = "Let me think."
            if has_tools:
                message["tool_calls"] = [{
                    "id": "call_mock_1", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                }]
                finish = "tool_calls"
            self._json(200, {
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "model": model,
                "choices": [{"index": 0, "message": message, "finish_reason": finish}],
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
