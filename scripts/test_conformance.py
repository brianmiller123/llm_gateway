#!/usr/bin/env python3
"""网关协议一致性测试（依据 DeepSeek 官方文档编写的客户端契约测试）。

参照文档：
- Responses API:  https://api-docs.deepseek.com/zh-cn/guides/responses_api
- Anthropic API:  https://api-docs.deepseek.com/zh-cn/guides/anthropic_api

前置环境（scripts/mock_upstream.py 提供 Chat 上游怪癖模式）：
  1. python3 scripts/mock_upstream.py                 # :9001
  2. GATEWAY_HTTP_REDIRECT=false GATEWAY_AUTH_MODE=none ./target/debug/llm_gateway  # :8080
  3. python3 scripts/test_conformance.py

用法:
  python3 scripts/test_conformance.py [--base http://127.0.0.1:8080] [--model mock-1]
  python3 scripts/test_conformance.py --only responses   # 只跑 Responses(API) 节
  python3 scripts/test_conformance.py --only anthropic
"""
import argparse
import json
import os
import sys
import urllib.error
import urllib.request

BASE = os.environ.get("GATEWAY_URL", "http://127.0.0.1:8080")
MODEL = os.environ.get("GATEWAY_MODEL", "mock-1")
API_KEY = os.environ.get("GATEWAY_API_KEY", "sk-test")
MODE_FILE = "/tmp/mock_mode.txt"

PASS = 0
FAIL = 0


def set_mode(mode: str) -> None:
    with open(MODE_FILE, "w") as f:
        f.write(mode)


def last_upstream_request() -> dict:
    with open("/tmp/mock_last_request.json") as f:
        return json.load(f)["body"]


def check(name: str, cond: bool, detail: str = "") -> None:
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"  PASS  {name}")
    else:
        FAIL += 1
        print(f"  FAIL  {name}  {detail}")

def raw_sse(path: str, payload: dict) -> tuple[int, list[tuple[str, dict]]]:
    """POST 并解析 SSE 事件流，返回 (http_status, [(event, data_json)])。"""
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json", "Accept": "text/event-stream",
                 "Authorization": f"Bearer {API_KEY}"},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            status, raw = resp.status, resp.read().decode()
            ctype = resp.headers.get("Content-Type", "")
    except urllib.error.HTTPError as e:
        return e.code, []
    events: list[tuple[str, dict]] = []
    if "text/event-stream" not in ctype:
        return status, events
    for block in raw.replace("\r\n\r\n", "\n\n").split("\n\n"):
        ev, data = None, None
        for line in block.splitlines():
            if line.startswith("event:"):
                ev = line[6:].strip()
            elif line.startswith("data:"):
                data = line[5:].strip()
        if ev and data:
            try:
                events.append((ev, json.loads(data)))
            except json.JSONDecodeError:
                events.append((ev, {"_raw": data}))
    return status, events


# ---------------------------------------------------------------------------
# Responses API（Codex 客户端契约 — DeepSeek responses_api 文档）
# ---------------------------------------------------------------------------

def responses_section() -> None:
    from openai import APIStatusError

    from openai import OpenAI

    client = OpenAI(api_key=API_KEY, base_url=f"{BASE}/v1")

    print("\n[Responses API] 非流式")
    set_mode("")
    r = client.responses.create(model=MODEL, instructions="You are a helpful assistant.",
                                input="Hi, how are you?", max_output_tokens=1024)
    check("output_text 与上游一致", r.output_text == "Hello world!", repr(r.output_text))
    check("status == completed", r.status == "completed", str(r.status))
    check("id 以 resp_ 开头", r.id.startswith("resp_"), r.id)
    check("usage 携带 input/output tokens",
          r.usage is not None and r.usage.input_tokens == 12 and r.usage.output_tokens == 7,
          str(r.usage))
    check("instructions 进 system 消息",
          last_upstream_request()["messages"][0] == {"role": "system", "content": "You are a helpful assistant."})
    out_req = last_upstream_request()
    check("input 字符串进 user 消息",
          out_req["messages"][1] == {"role": "user", "content": "Hi, how are you?"})
    check("非 o 系列模型输出上限映射为 max_tokens",
          "max_tokens" in out_req and out_req["max_tokens"] == 1024,
          json.dumps({k: out_req[k] for k in out_req if "token" in k}))

    print("\n[Responses API] 流式事件序（文档：created→item.added→content_part.added→text.delta→done→item.done→completed，无 [DONE] 帧）")
    status, evs = raw_sse("/v1/responses", {"model": MODEL, "input": "Hi", "stream": True})
    types = [t for t, _ in evs]
    check("HTTP 200 + SSE", status == 200, str(status))
    check("首事件 response.created", types and types[0] == "response.created", str(types))
    check("含 output_item.added(message)", "response.output_item.added" in types)
    check("含 content_part.added（文档事件清单）", "response.content_part.added" in types)
    check("含 ≥2 个 output_text.delta", types.count("response.output_text.delta") >= 2)
    check("文本事件后随 output_text.done",
          types.index("response.output_text.done") > types.index("response.output_text.delta")
          if "response.output_text.done" in types else False)
    check("含 content_part.done", "response.content_part.done" in types)
    check("末事件 response.completed", types and types[-1] == "response.completed", str(types))
    completed = next((d for t, d in evs if t == "response.completed"), {})
    check("completed 携带完整 response + usage",
          completed.get("response", {}).get("usage", {}).get("input_tokens") == 12,
          json.dumps(completed.get("response", {}).get("usage")))

    print("\n[Responses API] 推理模型（reasoning item + reasoning 事件，DeepSeek 推理模型）")
    set_mode("reasoning_first_stream")
    stream = client.responses.create(model=MODEL, input="Hi", stream=True)
    reasoning_text, output_text, ev_types = "", "", []
    for event in stream:
        ev_types.append(event.type)
        if event.type == "response.reasoning_summary_text.delta":
            reasoning_text += event.delta
        if event.type == "response.output_text.delta":
            output_text += event.delta
    check("reasoning item 事件存在", "response.reasoning_summary_part.added" in ev_types, str(ev_types))
    check("reasoning 文本完整", reasoning_text == "Let me think.", repr(reasoning_text))
    check("正文完整", output_text == "Hello world!", repr(output_text))
    final = client.responses.create(model=MODEL, input="Hi")
    check("非流式 reasoning item 正文在 summary[]",
          any(getattr(o, "summary", None) and o.summary and o.summary[0].text == "Let me think."
              for o in final.output),
          str([(o.type, getattr(o, "summary", None)) for o in final.output]))
    set_mode("")

    print("\n[Responses API] 工具调用往返（function 工具，文档 Tools: function 支持）")
    tools = [{
        "type": "function",
        "name": "get_weather",
        "description": "Get weather",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    }]
    r = client.responses.create(model=MODEL, input="Weather in Paris?", tools=tools)
    fc = next((o for o in r.output if o.type == "function_call"), None)
    check("非流式产出 function_call item", fc is not None, str([o.type for o in r.output]))
    if fc:
        check("function_call.name/arguments 正确",
              fc.name == "get_weather" and json.loads(fc.arguments) == {"city": "Paris"},
              f"{fc.name} {fc.arguments}")
    status, evs = raw_sse("/v1/responses", {"model": MODEL, "input": "Weather?", "stream": True, "tools": tools})
    types = [t for t, _ in evs]
    check("流式产出 function_call_arguments.delta", "response.function_call_arguments.delta" in types, str(types))
    check("流式末事件仍为 completed", types and types[-1] in ("response.completed", "response.incomplete"), str(types))
    # 回传 function_call_output（文档输入 item）
    fc_out = next((o for o in r.output if o.type == "function_call"), None)
    if fc_out:
        r2 = client.responses.create(model=MODEL, input=[
            {"role": "user", "content": "Weather in Paris?"},
            {"type": "function_call", "call_id": fc_out.call_id, "name": "get_weather", "arguments": fc_out.arguments},
            {"type": "function_call_output", "call_id": fc_out.call_id, "output": "15 degrees"},
        ], tools=tools)
        check("function_call_output 回传成功", r2.output_text == "Hello world!", repr(r2.output_text))
        msgs = last_upstream_request()["messages"]
        check("历史映射含 role:tool 消息",
              any(m.get("role") == "tool" and m.get("tool_call_id") == fc_out.call_id for m in msgs),
              json.dumps([m.get("role") for m in msgs]))

    print("\n[Responses API] tool_choice 形态（文档：none/auto/required/指定工具）")
    client.responses.create(model=MODEL, input="Hi", tools=tools, tool_choice="required")
    check("tool_choice=required 直传", last_upstream_request().get("tool_choice") == "required")
    client.responses.create(model=MODEL, input="Hi", tools=tools, tool_choice={"type": "function", "name": "get_weather"})
    tc = last_upstream_request().get("tool_choice")
    check("指定工具 → Chat 嵌套形态", tc == {"type": "function", "function": {"name": "get_weather"}}, json.dumps(tc))

    print("\n[Responses API] 截断（文档：response.incomplete 携带完整 response）")
    set_mode("length_stream")
    status, evs = raw_sse("/v1/responses", {"model": MODEL, "input": "Hi", "stream": True})
    types = [t for t, _ in evs]
    check("末事件 response.incomplete", types and types[-1] == "response.incomplete", str(types))
    inc = next((d for t, d in evs if t == "response.incomplete"), {})
    check("incomplete_details.reason == max_output_tokens",
          inc.get("response", {}).get("incomplete_details", {}).get("reason") == "max_output_tokens",
          json.dumps(inc.get("response", {}).get("incomplete_details")))
    r = client.responses.create(model=MODEL, input="Hi")
    check("非流式截断 status == incomplete", r.status == "incomplete", str(r.status))
    check("截断正文保留", r.output_text == "partial out", repr(r.output_text))

    print("\n[Responses API] 错误必须可见（cc-switch 哲学：错误不得伪装成功）")
    set_mode("")
    for stream in (False, True):
        try:
            kw = {"model": MODEL, "input": "boom please"}
            if stream:
                kw["stream"] = True
                it = client.responses.create(**kw)
                consumed = [e.type for e in it]
                check(f"流式={stream} boom 抛错（非静默空）", False, str(consumed))
            else:
                r = client.responses.create(**kw)
                check(f"流式={stream} boom 抛错（非静默空）", r.output_text != "", repr(r.output_text))
        except APIStatusError as e:
            check(f"流式={stream} boom 抛错（非静默空）", e.status_code >= 400, f"HTTP {e.status_code}")

    print("\n[Responses API] 上游怪癖回归（数组增量 / 字符串 usage / 完整 JSON 回体）")
    set_mode("content_parts_stream")
    r = client.responses.create(model=MODEL, input="Hi")
    check("content-parts 数组增量 → 文本完整", r.output_text == "Hello world!", repr(r.output_text))
    set_mode("string_usage_stream")
    acc = ""
    for event in client.responses.create(model=MODEL, input="Hi", stream=True):
        if event.type == "response.output_text.delta":
            acc += event.delta
    check("字符串 usage 不再丢帧", acc == "Hello world!", repr(acc))
    set_mode("json_body_for_stream")
    acc = ""
    for event in client.responses.create(model=MODEL, input="Hi", stream=True):
        if event.type == "response.output_text.delta":
            acc += event.delta
    check("上游忽略 stream 参数 → 合成事件流", acc == "Full JSON reply!", repr(acc))
    set_mode("")

    print("\n[Responses API] P0-2 priming 窗口：次块错误 envelope 换家而非透传 failed")
    set_mode("error_second_chunk_stream")
    status, evs = raw_sse("/v1/responses", {"model": MODEL, "input": "Hi", "stream": True})
    types = [t for t, _ in evs]
    # 次块错误发生在任何产出之前 → 网关必须换家/拒绝，客户端不得看到 failed 流
    check("次块错误不产出 response.failed", "response.failed" not in types, str(types))
    check("次块错误无任何输出文本", not any(t.startswith("response.output_text") for t in types), str(types))
    set_mode("")

    print("\n[Responses API] P1-2 stream_options 合并：客户端自带对象仍强制 include_usage")
    client.responses.create(model=MODEL, input="Hi", stream=True, stream_options={"include_usage": False, "extra": 1})
    so = last_upstream_request().get("stream_options")
    check("既有 stream_options 被合并 include_usage=true",
          so == {"include_usage": True, "extra": 1}, json.dumps(so))

    print("\n[Responses API] P0-1 previous_response_id 桥接：转换路径历史恢复")
    set_mode("")
    r1 = client.responses.create(model=MODEL, input="First turn", max_output_tokens=1024)
    rid = r1.id
    check("首轮 id 为 resp_ 网关形态", rid.startswith("resp_"), rid)
    # 第二轮带 previous_response_id + 仅新增消息 → 网关应把首轮 output 前插进 input
    r2 = client.responses.create(model=MODEL, previous_response_id=rid, input="Second turn")
    msgs = last_upstream_request()["messages"]
    check("历史桥接：恢复的 assistant 消息在前",
          msgs[0]["role"] == "assistant" and msgs[0]["content"] == "Hello world!",
          json.dumps(msgs))
    check("历史桥接：客户端新增消息在后",
          msgs[-1] == {"role": "user", "content": "Second turn"}, json.dumps(msgs))
    # 未知 previous_response_id → 显式 400（不静默丢上下文）
    try:
        client.responses.create(model=MODEL, previous_response_id="resp_0000000000000000", input="x")
        check("未知 previous_response_id 显式 400", False, "无异常")
    except APIStatusError as e:
        check("未知 previous_response_id 显式 400", e.status_code == 400, f"HTTP {e.status_code}")
    set_mode("")


def _has_image_url_in_messages(body: dict) -> bool:
    """出站 Chat 体消息内容是否仍含 image_url part（整流验证用）"""
    for m in body.get("messages", []):
        content = m.get("content")
        if isinstance(content, list):
            if any(isinstance(c, dict) and c.get("type") == "image_url" for c in content):
                return True
    return False


# ---------------------------------------------------------------------------
# 工程层回归（网关自身能力：custom 工具 / 缺 index 帧 / 首帧错误 / 整流 / gzip）
# ---------------------------------------------------------------------------

def engineering_section() -> None:
    from openai import APIStatusError
    from openai import OpenAI

    client = OpenAI(api_key=API_KEY, base_url=f"{BASE}/v1")

    print("\n[工程] custom 工具往返（custom_tool_call 事件）")
    set_mode("")
    tools = [{"type": "custom", "name": "get_weather",
              "description": "Get weather", "format": {"type": "text"}}]
    r = client.responses.create(model=MODEL, input="Weather in Paris?", tools=tools)
    ct = next((o for o in r.output if o.type == "custom_tool_call"), None)
    check("custom 工具产出 custom_tool_call", ct is not None, str([o.type for o in r.output]))
    if ct:
        check("custom_tool_call 入参还原（input 字符串）",
              ct.name == "get_weather" and json.loads(ct.input).get("city") == "Paris",
              f"{ct.name} {getattr(ct, 'input', None)}")
    status, evs = raw_sse("/v1/responses", {"model": MODEL, "input": "Weather?",
                                             "stream": True, "tools": tools})
    types = [t for t, _ in evs]
    check("流式 custom_tool_call_input.delta/done 事件",
          "response.custom_tool_call_input.delta" in types
          and "response.custom_tool_call_input.done" in types, str(types))

    print("\n[工程] 工具帧缺 index（H4 回归：并行工具帧 key 解析）")
    set_mode("no_index_tool_stream")
    status, evs = raw_sse("/v1/responses", {
        "model": MODEL, "input": "Weather?", "stream": True,
        "tools": [{"type": "function", "name": "get_weather",
                   "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}],
    })
    args = "".join(d.get("delta", "") for t, d in evs if t == "response.function_call_arguments.delta")
    check("无 index 工具帧参数完整聚合",
          json.loads(args) == {"city": "Paris"} if args else False, repr(args))
    set_mode("")

    print("\n[工程] 200 + 首帧错误 envelope（M1 回归：不得透传垃圾帧）")
    set_mode("error_first_chunk_stream")
    status, evs = raw_sse("/v1/responses", {"model": MODEL, "input": "Hi", "stream": True})
    check("首帧错误 → 网关非 200 拒绝", status >= 400, f"status={status}")
    set_mode("")

    print("\n[工程] 图片请求反应式整流（H6 回归：400 仅文本 → 重试成功）")
    set_mode("rectify_image_trigger")
    try:
        r = client.responses.create(model=MODEL, input=[
            {"role": "user", "content": [
                {"type": "input_text", "text": "Describe this"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
            ]}])
        check("整流重试后成功", r.output_text == "Hello world!", repr(r.output_text))
        up = last_upstream_request()
        check("重试出站体图片已替换", not _has_image_url_in_messages(up),
              json.dumps(up.get("messages"))[:200])
    except APIStatusError as e:
        check("整流重试后成功", False, f"HTTP {e.status_code}")
    set_mode("")

    print("\n[工程] 上游 gzip 响应（reqwest 自动解压回归）")
    set_mode("gzip_response")
    r = client.responses.create(model=MODEL, input="Hi")
    check("gzip 响应体解压成功", r.output_text == "gzip decoded reply", repr(r.output_text))
    set_mode("")
# ---------------------------------------------------------------------------
# Anthropic API（Claude Code 客户端契约 — DeepSeek anthropic_api 文档）
# ---------------------------------------------------------------------------

def anthropic_section() -> None:
    import anthropic
    from anthropic import APIStatusError

    client = anthropic.Anthropic(api_key=API_KEY, base_url=BASE)

    print("\n[Anthropic API] 非流式（文档：content 数组 / stop_reason / usage）")
    set_mode("")
    m = client.messages.create(
        model=MODEL, max_tokens=100, system="You are a helpful assistant.",
        messages=[{"role": "user", "content": [{"type": "text", "text": "Hi, how are you?"}]}],
    )
    check("text 块内容正确", m.content and m.content[0].type == "text" and m.content[0].text == "Hello world!",
          str(m.content))
    check("stop_reason == end_turn", m.stop_reason == "end_turn", str(m.stop_reason))
    check("usage input/output tokens", m.usage.input_tokens == 12 and m.usage.output_tokens == 7, str(m.usage))
    check("system 进 system 消息（剥信封）",
          last_upstream_request()["messages"][0] == {"role": "system", "content": "You are a helpful assistant."})
    m = client.messages.create(
        model=MODEL, max_tokens=100,
        system=[{"type": "text", "text": "part one"}, {"type": "text", "text": "part two"}],
        messages=[{"role": "user", "content": "Hi"}],
    )
    check("system 数组形态支持", m.content[0].text == "Hello world!", str(m.content))
    client.messages.create(
        model=MODEL, max_tokens=100,
        messages=[{"role": "user", "content": "Hi"}],
        stop_sequences=["END"], temperature=0.5, top_p=0.9,
    )
    up = last_upstream_request()
    check("stop_sequences → stop", up.get("stop") == ["END"], json.dumps(up.get("stop")))
    check("temperature/top_p 直传", up.get("temperature") == 0.5 and up.get("top_p") == 0.9)
    check("max_tokens 映射（非 o 系列保留 max_tokens）", up.get("max_tokens") == 100,
          json.dumps({k: up[k] for k in up if "token" in k}))

    print("\n[Anthropic API] 流式事件序（message_start→block_start→delta→block_stop→message_delta→message_stop）")
    status, evs = raw_sse("/v1/messages", {
        "model": MODEL, "max_tokens": 100, "stream": True,
        "messages": [{"role": "user", "content": "Hi"}],
    })
    types = [t for t, _ in evs]
    check("HTTP 200 + SSE", status == 200, str(status))
    check("首事件 message_start", types and types[0] == "message_start", str(types))
    check("content_block_start(text)", "content_block_start" in types)
    check("text_delta 事件", "content_block_delta" in types)
    check("content_block_stop", "content_block_stop" in types)
    check("message_delta 携带 stop_reason",
          any(t == "message_delta" and d["delta"].get("stop_reason") == "end_turn" for t, d in evs),
          str([d for t, d in evs if t == "message_delta"]))
    check("message_delta 携带 usage.output_tokens",
          any(t == "message_delta" and d.get("usage", {}).get("output_tokens") == 7 for t, d in evs))
    check("末事件 message_stop", types and types[-1] == "message_stop", str(types))

    print("\n[Anthropic API] thinking（文档 Message Fields: thinking Supported）")
    set_mode("reasoning_first_stream")
    m = client.messages.create(model=MODEL, max_tokens=200,
                               messages=[{"role": "user", "content": "think then answer"}])
    check("thinking 块在最前", m.content and m.content[0].type == "thinking", str([b.type for b in m.content]))
    check("thinking 文本完整", m.content[0].thinking == "Let me think.", str(m.content[0].thinking))
    status, evs = raw_sse("/v1/messages", {
        "model": MODEL, "max_tokens": 200, "stream": True,
        "messages": [{"role": "user", "content": "think then answer"}],
    })
    types = [t for t, _ in evs]
    check("流式 thinking_delta 事件", "content_block_delta" in types and
          any(d["delta"].get("type") == "thinking_delta" for t, d in evs if t == "content_block_delta"),
          str(types))
    check("thinking 块先于 text 块",
          next(i for i, (t, d) in enumerate(evs) if t == "content_block_start" and d["content_block"]["type"] == "thinking")
          < next(i for i, (t, d) in enumerate(evs) if t == "content_block_start" and d["content_block"]["type"] == "text"))
    # thinking 历史回传（thinking 块 + signature 字段被网关剥离）
    m2 = client.messages.create(model=MODEL, max_tokens=200, messages=[
        {"role": "user", "content": "q1"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "old thought", "signature": "sig-abc"},
            {"type": "text", "text": "old answer"},
        ]},
        {"role": "user", "content": "q2"},
    ])
    check("thinking 历史回传可用", m2.content[-1].type == "text" and m2.content[-1].text == "Hello world!",
          str(m2.content))
    up = last_upstream_request()
    asst = next(m for m in up["messages"] if m["role"] == "assistant")
    # 非 reasoning 上游（mock）：thinking 块整体剥离（signature 不泄漏），仅文本保留。
    # reasoning_content 映射仅对 deepseek/mimo 等厂商生效（REASONING_VENDOR_HINTS）
    check("thinking 历史 → 文本保留、signature/thinking 剥离（非 reasoning 上游）",
          asst.get("content") == "old answer"
          and "signature" not in json.dumps(asst)
          and "thinking" not in json.dumps(asst),
          json.dumps(asst))
    set_mode("")

    print("\n[Anthropic API] 工具往返（tool_use / tool_result，文档 Tool Fields）")
    tools = [{"name": "get_weather", "description": "Get weather",
              "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}]
    m = client.messages.create(model=MODEL, max_tokens=200, tools=tools,
                               messages=[{"role": "user", "content": "Weather in Paris?"}])
    tu = next((b for b in m.content if b.type == "tool_use"), None)
    check("tool_use 块产出", tu is not None, str([b.type for b in m.content]))
    if tu:
        check("tool_use name/input 正确", tu.name == "get_weather" and tu.input == {"city": "Paris"},
              f"{tu.name} {tu.input}")
    check("stop_reason == tool_use", m.stop_reason == "tool_use", str(m.stop_reason))
    up = last_upstream_request()
    chat_tool = up["tools"][0]
    check("input_schema → parameters", chat_tool["function"]["parameters"]["properties"]["city"]["type"] == "string",
          json.dumps(chat_tool))
    status, evs = raw_sse("/v1/messages", {
        "model": MODEL, "max_tokens": 200, "stream": True, "tools": tools,
        "messages": [{"role": "user", "content": "Weather?"}],
    })
    starts = [d["content_block"] for t, d in evs if t == "content_block_start" and d["content_block"]["type"] == "tool_use"]
    deltas = [d["delta"] for t, d in evs if t == "content_block_delta" and d["delta"]["type"] == "input_json_delta"]
    check("流式 tool_use block + input_json_delta",
          bool(starts) and bool(deltas), f"{starts} {deltas}")
    # tool_result 回传（content 字符串与数组两种形态；is_error 忽略）
    if tu:
        m2 = client.messages.create(model=MODEL, max_tokens=200, tools=tools, messages=[
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": tu.id, "name": tu.name, "input": tu.input}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tu.id, "is_error": False,
                 "content": [{"type": "text", "text": "15 degrees"}]},
            ]},
        ])
        check("tool_result 数组形态回传成功", m2.content[0].type == "text", str(m2.content))
        up = last_upstream_request()
        tool_msg = next(m for m in up["messages"] if m["role"] == "tool")
        check("tool_result → role:tool + content 文本",
              tool_msg["tool_call_id"] == tu.id and tool_msg["content"] == "15 degrees",
              json.dumps(tool_msg))

    print("\n[Anthropic API] tool_choice（文档：none/auto/any/tool 全支持）")
    for choice, expect in [("none", "none"), ({"type": "any"}, "required"),
                           ({"type": "tool", "name": "get_weather"}, {"type": "function", "function": {"name": "get_weather"}})]:
        client.messages.create(model=MODEL, max_tokens=100, tools=tools, tool_choice=choice,
                               messages=[{"role": "user", "content": "Hi"}])
        check(f"tool_choice {json.dumps(choice)} → {json.dumps(expect)}",
              last_upstream_request().get("tool_choice") == expect,
              json.dumps(last_upstream_request().get("tool_choice")))

    print("\n[Anthropic API] 缓存三桶（usage: input + cache_read + cache_creation 恒等式）")
    set_mode("cache_usage_stream")
    status, evs = raw_sse("/v1/messages", {
        "model": MODEL, "max_tokens": 100, "stream": True,
        "messages": [{"role": "user", "content": "Hi"}],
    })
    delta_usage = next((d.get("usage", {}) for t, d in evs if t == "message_delta"), {})
    check("input_tokens 剔除缓存命中（100−40=60）", delta_usage.get("input_tokens") == 60, json.dumps(delta_usage))
    check("cache_read_input_tokens == 40", delta_usage.get("cache_read_input_tokens") == 40, json.dumps(delta_usage))

    print("\n[Anthropic API] 错误必须可见（200+错误体不得伪装成空 Message）")
    set_mode("")
    for stream in (False, True):
        try:
            kw = dict(model=MODEL, max_tokens=100, messages=[{"role": "user", "content": "boom please"}])
            if stream:
                text = ""
                with client.messages.stream(model=MODEL, max_tokens=100,
                                            messages=[{"role": "user", "content": "boom please"}]) as s:
                    for t in s.text_stream:
                        text += t
                check(f"流式={stream} boom 抛错（非静默空）", False, repr(text))
            else:
                m = client.messages.create(**kw)
                ok = any(b.type == "text" and b.text for b in m.content) if m.content else False
                check(f"流式={stream} boom 抛错（非静默空）", False, str(m.content))
        except APIStatusError as e:
            check(f"流式={stream} boom 抛错（非静默空）", e.status_code >= 400, f"HTTP {e.status_code}")
        except anthropic.APIError as e:
            check(f"流式={stream} boom 抛错（非静默空）", True, str(e))

    print("\n[Anthropic API] 上游怪癖回归")
    set_mode("string_usage_stream")
    text = ""
    with client.messages.stream(model=MODEL, max_tokens=100, messages=[{"role": "user", "content": "Hi"}]) as s:
        for t in s.text_stream:
            text += t
    check("字符串 usage 不再丢帧", text == "Hello world!", repr(text))
    set_mode("unnamed_tool_stream")
    status, evs = raw_sse("/v1/messages", {
        "model": MODEL, "max_tokens": 100, "stream": True,
        "messages": [{"role": "user", "content": "Hi"}],
    })
    types = [t for t, _ in evs]
    starts = [d["content_block"] for t, d in evs
              if t == "content_block_start" and d["content_block"]["type"] == "tool_use"]
    check("无名工具帧 → 合成 unknown_tool 块（id 保留，不伪装空成功）",
          any(b.get("name") == "unknown_tool" and b.get("id") == "call_x" for b in starts),
          str(types))
    set_mode("")


def main() -> None:
    global BASE, MODEL
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default=BASE)
    ap.add_argument("--model", default=MODEL)
    ap.add_argument("--only", choices=["responses", "anthropic", "engineering"])
    args = ap.parse_args()
    BASE, MODEL = args.base, args.model

    if args.only in (None, "responses"):
        responses_section()
    if args.only in (None, "anthropic"):
        anthropic_section()
    if args.only in (None, "engineering"):
        engineering_section()

    print(f"\n== {PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
