#!/usr/bin/env python3
"""Coding Plan 模型作用域（model_scope）端到端测试。

覆盖场景：
  1. 写入校验：未知字段 / 中间通配 / 裸 * / 黑白名单同条目 / 拼写错误的精确模型名 → 400 明确报错
  2. 精确匹配：精确作用域 Plan 在其精确模型上压过更高 priority 的通配/不限模型 Plan
  3. 通配匹配：claude-* 覆盖家族全部模型（含不同变体）
  4. 黑白名单组合：deny 命中即排除（黑名单优先），被排除后回退次优 Plan
  5. 未配置作用域：model_scope 为 null 的 Plan 对所有模型生效（向后兼容）
  6. 模型切换动态生效：同一用户 ?model= 来回切换，激活 Plan 即时变化，无需重启
  7. PATCH 三态：对象=设置 / null=清空 / absent=保留；写后即时生效（服务端 reload）
  8. 审计：model_scope 变更写入审计日志（before/after 快照）

前置环境（参照 scripts/test_plan_members.py）：
  1. createdb 一个空库（迁移由网关启动时自动应用，0024 及以后含 model_scope）
  2. GATEWAY_HTTP_REDIRECT=false LDAP_URL= GATEWAY_DATABASE_URL=... ./target/debug/llm_gateway
  3. python3 scripts/test_plan_model_scope.py [--base http://127.0.0.1:18080]

说明：拼写报错用例依赖路由表非空且无 `*` 全捕获路由；不满足时自动跳过（脚本会
自建一个 disabled 供应商 + 三条路由作为兜底）。

环境变量：GATEWAY_ADMIN_USER（默认 admin）/ GATEWAY_ADMIN_PASSWORD（默认 admin12345）
"""
import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = os.environ.get("GATEWAY_URL", "http://127.0.0.1:18080")
ADMIN_USER = os.environ.get("GATEWAY_ADMIN_USER", "admin")
ADMIN_PASSWORD = os.environ.get("GATEWAY_ADMIN_PASSWORD", "admin12345")
SUFFIX = str(int(time.time()))  # 用户名/Plan 名唯一（库可复跑）

PASS = 0
FAIL = 0
ADMIN_TOKEN = ""
USER_TOKEN = ""


def check(name: str, cond: bool, detail: str = "") -> None:
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"  PASS  {name}")
    else:
        FAIL += 1
        print(f"  FAIL  {name}  {detail}")


def raw(method: str, path: str, payload=None, token: str | None = None,
        with_auth: bool = True) -> tuple[int, dict]:
    """发起请求，返回 (status, body_json)；非 JSON 错误体 → {}。"""
    headers = {"Content-Type": "application/json"}
    if with_auth and token:
        headers["Authorization"] = f"Bearer {token}"
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(BASE + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            body = resp.read().decode()
            return resp.status, (json.loads(body) if body else {})
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        try:
            return e.code, json.loads(body)
        except json.JSONDecodeError:
            return e.code, {}
    except urllib.error.URLError as e:
        return 0, {"error": {"message": str(e)}}


def api(method: str, path: str, payload=None) -> tuple[int, dict]:
    return raw(method, path, payload, token=ADMIN_TOKEN)


def my_plan(model: str | None = None) -> tuple[int, dict]:
    q = f"?model={urllib.parse.quote(model)}" if model else ""
    return raw("GET", f"/api/me/plan{q}", token=USER_TOKEN)


def err_msg(body: dict) -> str:
    return body.get("error", {}).get("message", "")


def create_plan(name: str, scope, priority: int = 1) -> tuple[int, dict, int | None]:
    payload = {
        "name": name, "description": "模型作用域 E2E", "priority": priority,
        "token_limit": "10M", "period_type": "monthly", "overage_action": "block",
        "alert_channels": ["in_site"], "enabled": True,
    }
    if scope is not None:
        payload["model_scope"] = scope
    st, body = api("POST", "/api/admin/plans", payload)
    return st, body, body.get("plan", {}).get("id")


def main() -> int:
    global ADMIN_TOKEN, USER_TOKEN
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default=BASE)
    args = ap.parse_args()
    globals()["BASE"] = args.base

    print("== 登录 ==")
    st, body = raw("POST", "/api/auth/login",
                   {"username": ADMIN_USER, "password": ADMIN_PASSWORD}, with_auth=False)
    check("管理员登录", st == 200 and "access_token" in body, f"{st} {body}")
    ADMIN_TOKEN = body.get("access_token", "")
    if not ADMIN_TOKEN:
        print("管理员登录失败，后续用例跳过")
        return 1

    # ---------- 路由兜底：拼写报错用例需要非空且无全捕获的路由表 ----------
    st, body = api("GET", "/api/admin/routes")
    routes = body.get("routes", []) if st == 200 else []
    created_provider = created_routes = None
    if not any(r.get("model_pattern") == "*" for r in routes):
        st, body = api("POST", "/api/admin/providers", {
            "name": f"e2e-scope-mock-{SUFFIX}", "api_type": "openai",
            "base_url": "http://127.0.0.1:9", "api_key": "sk-e2e-scope",
            "enabled": False,
        })
        if st in (200, 201):
            created_provider = body.get("provider", {}).get("id") or body.get("id")
            for pattern in ("claude-*", "gpt-4*", "gemini-*"):
                st, body = api("POST", "/api/admin/routes", {
                    "model_pattern": pattern, "provider_id": created_provider,
                    "priority": 100,
                })
                if st in (200, 201):
                    rid = body.get("route", {}).get("id") or body.get("id")
                    created_routes = (created_routes or []) + [rid]
    st, body = api("GET", "/api/admin/routes")
    routes = body.get("routes", []) if st == 200 else []
    # 拼写报错用例条件：路由表非空且无全捕获路由；不满足则跳过该用例
    routes_ready = bool(routes) and not any(r.get("model_pattern") == "*" for r in routes)

    # ---------- 准备：Plan ×3 / 用户 ----------
    print("== 准备：三个 Plan（精确 / 家族通配+黑名单 / 不限模型）+ 直连用户 ==")
    st_exact, body, pid_exact = create_plan(
        f"e2e-scope-exact-{SUFFIX}", {"allow": ["claude-sonnet-4-5"]}, priority=10)
    check("创建精确作用域 Plan（claude-sonnet-4-5, priority 10）",
          st_exact == 201 and pid_exact, f"{st_exact} {body}")
    st_fam, body, pid_fam = create_plan(
        f"e2e-scope-family-{SUFFIX}", {"allow": ["claude-*"], "deny": ["claude-2*"]},
        priority=5)
    check("创建家族作用域 Plan（claude-* 且排除 claude-2*）",
          st_fam == 201 and pid_fam, f"{st_fam} {body}")
    st_all, body, pid_all = create_plan(f"e2e-scope-all-{SUFFIX}", None, priority=1)
    check("创建不限模型 Plan（无 model_scope）", st_all == 201 and pid_all, f"{st_all} {body}")
    if not (pid_exact and pid_fam and pid_all):
        print("Plan 创建失败，后续用例跳过")
        return 1

    username = f"e2e_scope_user_{SUFFIX}"
    st, body = api("POST", "/api/admin/users",
                   {"username": username, "password": "pass12345"})
    check("创建用户", st in (200, 201), f"{st} {body}")
    uid = body.get("user", {}).get("id") or body.get("id")
    st, body = raw("POST", "/api/auth/login",
                   {"username": username, "password": "pass12345"}, with_auth=False)
    check("用户登录", st == 200, f"{st} {body}")
    USER_TOKEN = body.get("access_token", "")

    for pid in (pid_exact, pid_fam, pid_all):
        st, body = api("POST", f"/api/admin/plans/{pid}/users/add", {"user_ids": [uid]})
        check(f"直连加入 Plan {pid}", st == 200 and body.get("added") == 1, f"{st} {body}")

    # ---------- 1. 写入校验（非法模型名/结构明确报错） ----------
    print("== 写入校验 ==")
    st, body, _ = create_plan(f"e2e-scope-bad1-{SUFFIX}", {"white": ["claude-*"]})
    check("未知字段 white → 400", st == 400 and "white" in err_msg(body), f"{st} {body}")
    st, body, _ = create_plan(f"e2e-scope-bad2-{SUFFIX}", {"allow": ["gpt-*-mini"]})
    check("中间通配 gpt-*-mini → 400", st == 400 and "gpt-*-mini" in err_msg(body),
          f"{st} {body}")
    st, body, _ = create_plan(f"e2e-scope-bad3-{SUFFIX}", {"deny": ["*"]})
    check("裸 * → 400", st == 400, f"{st} {body}")
    st, body, _ = create_plan(
        f"e2e-scope-bad4-{SUFFIX}", {"allow": ["gpt-4o"], "deny": ["gpt-4o"]})
    check("黑白名单同条目 → 400", st == 400 and "never" in err_msg(body), f"{st} {body}")
    st, body, _ = create_plan(f"e2e-scope-bad5-{SUFFIX}", {"allow": ["claude sonnet"]})
    check("含空格的模型名 → 400", st == 400, f"{st} {body}")
    if routes_ready:
        # 拼写报错语义 = 精确条目不命中任何路由 pattern（前缀路由无法识别家族内
        # 拼写差异，故选一个完全未建路由的家族名）
        st, body, pid_typo = create_plan(
            f"e2e-scope-typo-{SUFFIX}", {"allow": ["kimi-k2-coder-typo"]})
        check("未建路由家族的精确模型名 → 400 unrecognized model",
              st == 400 and "unrecognized model" in err_msg(body), f"{st} {body}")
        if pid_typo:
            api("DELETE", f"/api/admin/plans/{pid_typo}")

    # ---------- 2. 配置回读 ----------
    print("== 配置回读 ==")
    st, body = api("GET", "/api/admin/plans")
    rows = {p["id"]: p for p in body.get("plans", [])}
    check("列表返回 model_scope（归一化小写）",
          rows.get(pid_fam, {}).get("model_scope")
          == {"allow": ["claude-*"], "deny": ["claude-2*"]},
          f"{rows.get(pid_fam, {}).get('model_scope')}")
    check("未配置作用域回读为 null",
          rows.get(pid_all, {}).get("model_scope") is None,
          f"{rows.get(pid_all, {})}")

    # ---------- 3. 按模型解析（作用域感知） ----------
    print("== 按模型解析 ==")
    st, body = my_plan("claude-sonnet-4-5")
    check("精确模型命中精确作用域 Plan（specificity 压过 priority）",
          st == 200 and body.get("plan", {}).get("name") == f"e2e-scope-exact-{SUFFIX}",
          f"{st} {body.get('plan', {}).get('name')}")
    check("响应携带解析后的 model_scope",
          body.get("plan", {}).get("model_scope") == {"allow": ["claude-sonnet-4-5"]},
          f"{body.get('plan', {}).get('model_scope')}")
    st, body = my_plan("claude-opus-4-1")
    check("家族其他模型命中 claude-* Plan",
          body.get("plan", {}).get("name") == f"e2e-scope-family-{SUFFIX}",
          f"{body.get('plan', {}).get('name')}")
    st, body = my_plan("claude-2-opus")
    check("黑名单命中后回退不限模型 Plan",
          body.get("plan", {}).get("name") == f"e2e-scope-all-{SUFFIX}",
          f"{body.get('plan', {}).get('name')}")
    st, body = my_plan("gpt-4o")
    check("白名单外模型 → 不限模型 Plan",
          body.get("plan", {}).get("name") == f"e2e-scope-all-{SUFFIX}",
          f"{body.get('plan', {}).get('name')}")
    st, body = my_plan("GPT-4O")
    check("大小写不敏感", body.get("plan", {}).get("name") == f"e2e-scope-all-{SUFFIX}",
          f"{body.get('plan', {}).get('name')}")

    # ---------- 4. 模型切换动态生效 ----------
    print("== 模型切换动态生效 ==")
    _, b1 = my_plan("claude-sonnet-4-5")
    _, b2 = my_plan("gpt-4o")
    _, b3 = my_plan("claude-sonnet-4-5")
    check("同会话切换模型激活 Plan 即时变化",
          b1.get("plan", {}).get("id") != b2.get("plan", {}).get("id")
          and b1.get("plan", {}).get("id") == b3.get("plan", {}).get("id"),
          f"{b1.get('plan', {}).get('id')} / {b2.get('plan', {}).get('id')}")

    # ---------- 5. 无模型参数（存量语义：作用域不参与） ----------
    print("== 无模型参数 ==")
    st, body = my_plan()
    check("缺省解析回存量 (priority, plan_id) 语义",
          st == 200 and body.get("plan", {}).get("name") == f"e2e-scope-exact-{SUFFIX}",
          f"{body.get('plan', {}).get('name')}")

    # ---------- 6. PATCH 三态与即时生效 ----------
    print("== PATCH 三态 ==")
    st, body = api("PATCH", f"/api/admin/plans/{pid_exact}",
                   {"model_scope": None})
    check("PATCH model_scope=null → 清空", st == 200
          and body.get("plan", {}).get("model_scope") is None, f"{st} {body}")
    st, body = my_plan("claude-sonnet-4-5")
    check("清空后即时生效：精确 Plan 退出 claude 竞争 → 家族 Plan 胜出",
          body.get("plan", {}).get("name") == f"e2e-scope-family-{SUFFIX}",
          f"{body.get('plan', {}).get('name')}")
    st, body = api("PATCH", f"/api/admin/plans/{pid_exact}", {"priority": 11})
    check("PATCH absent=保留（仅改 priority）", st == 200
          and body.get("plan", {}).get("model_scope") is None, f"{st} {body}")
    st, body = api("PATCH", f"/api/admin/plans/{pid_exact}",
                   {"model_scope": {"allow": ["GPT-4O"]}})
    got = body.get("plan", {}).get("model_scope")
    check("PATCH 设置新作用域并归一化小写", st == 200
          and got == {"allow": ["gpt-4o"]}, f"{st} {got}")
    st, body = my_plan("gpt-4o")
    check("新作用域即时生效：gpt-4o 改命中原精确 Plan（specificity 2）",
          body.get("plan", {}).get("name") == f"e2e-scope-exact-{SUFFIX}",
          f"{body.get('plan', {}).get('name')}")

    # ---------- 6b. downgrade_model 三态（同款 double_option 修复） ----------
    print("== PATCH downgrade_model 三态 ==")
    if routes_ready:
        pid_dm = create_plan(f"e2e-scope-dm-{SUFFIX}", None, priority=1)[2]
        st, body = api("PATCH", f"/api/admin/plans/{pid_dm}", {
            "overage_action": "downgrade", "downgrade_model": "claude-sonnet-4-5"})
        check("设置降级目标", st == 200
              and body.get("plan", {}).get("downgrade_model") == "claude-sonnet-4-5",
              f"{st} {body.get('plan', {}).get('downgrade_model')}")
        st, body = api("PATCH", f"/api/admin/plans/{pid_dm}", {"downgrade_model": None})
        check("downgrade 计划清空目标 → 400（策略仍需目标）",
              st == 400 and "requires downgrade_model" in err_msg(body), f"{st} {body}")
        st, body = api("PATCH", f"/api/admin/plans/{pid_dm}", {"downgrade_model": ""})
        check("downgrade 计划置空串目标 → 400",
              st == 400, f"{st} {body}")
        st, body = api("PATCH", f"/api/admin/plans/{pid_dm}",
                       {"overage_action": "block", "downgrade_model": None})
        check("切回 block 并 null → 清空残留目标（修复点）", st == 200
              and body.get("plan", {}).get("downgrade_model") is None,
              f"{st} {body.get('plan', {}).get('downgrade_model')}")
        st, body = api("PATCH", f"/api/admin/plans/{pid_dm}",
                       {"overage_action": "downgrade"})
        check("无目标时切 downgrade → 400",
              st == 400 and "requires downgrade_model" in err_msg(body), f"{st} {body}")
        api("DELETE", f"/api/admin/plans/{pid_dm}")
    else:
        print("  SKIP 无路由环境，downgrade 用例需可路由目标")

    # ---------- 7. 审计留痕 ----------
    print("== 审计 ==")
    st, body = api("GET", "/api/admin/audit?action=plan.update&limit=10")
    hits = [a for a in body.get("audit_logs", [])
            if "model_scope" in json.dumps(a.get("detail") or {})]
    check("model_scope 变更写入审计", st == 200 and hits, f"{st} {str(body)[:200]}")

    # ---------- 清理 ----------
    print("== 清理 ==")
    for pid in (pid_exact, pid_fam, pid_all):
        api("DELETE", f"/api/admin/plans/{pid}")
    if created_routes:
        for rid in created_routes:
            api("DELETE", f"/api/admin/routes/{rid}")
    if created_provider:
        api("DELETE", f"/api/admin/providers/{created_provider}")
    print("已清理测试 Plan / 兜底路由 / 兜底供应商（测试用户保留，名称含时间戳可复跑）")

    print(f"\n结果: {PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
