#!/usr/bin/env python3
"""Coding Plan 成员管理（直连用户 / 加入分组）端到端测试。

覆盖场景：
  1. 正常添加：批量添加用户/分组 → 计数与列表正确，选择器 is_member 标记
  2. 重复添加：409 conflict + 可读成员名列表；全有或全无（混合批次不落库）
  3. 搜索为空：无命中返回空列表；模糊搜索（用户名/邮箱/组名）命中
  4. 参数错误：空 user_ids/group_ids、不存在的 id、不存在的 plan → 400
  5. 无权限：未登录 → 401 unauthorized；普通用户 → 403 forbidden
  6. 运行时生效：直连/分组双通道用户 /api/me/plan 即时生效与移除后回退
  7. 移除成员：计数正确；Plan 删除返回解绑分组数与直连用户数

前置环境（参照 scripts/test_conformance.py 的启动方式）：
  1. createdb 一个空库（迁移由网关启动时自动应用）
  2. GATEWAY_HTTP_REDIRECT=false LDAP_URL= GATEWAY_DATABASE_URL=... ./target/debug/llm_gateway
  3. python3 scripts/test_plan_members.py [--base http://127.0.0.1:18080]

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
SUFFIX = str(int(time.time()))  # 用户名/组名/Plan 名唯一（库可复跑）

PASS = 0
FAIL = 0
ADMIN_TOKEN = ""
USER_TOKENS = {}


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


def api(method: str, path: str, payload=None, who: str = "admin"):
    return raw(method, path, payload, token=USER_TOKENS.get(who, ADMIN_TOKEN))


def err_code(body: dict) -> str:
    return body.get("error", {}).get("code", "")


def err_msg(body: dict) -> str:
    return body.get("error", {}).get("message", "")


def main() -> int:
    global ADMIN_TOKEN
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

    # ---------- 准备数据 ----------
    print("== 准备：Plan / 用户 / 分组 ==")
    plan_name = f"e2e-pm-{SUFFIX}"
    st, body = api("POST", "/api/admin/plans", {
        "name": plan_name, "description": "成员管理 E2E", "priority": 5,
        "token_limit": "10M", "period_type": "monthly", "overage_action": "block",
        "alert_channels": ["in_site"], "enabled": True,
    })
    check("创建 Plan", st == 201 and body.get("plan", {}).get("name") == plan_name, f"{st} {body}")
    plan_id = body.get("plan", {}).get("id")

    users = {}
    for name in ("alice", "bob", "carol"):
        username = f"e2e_pm_{name}_{SUFFIX}"
        st, body = api("POST", "/api/admin/users",
                       {"username": username, "password": "pass12345"})
        check(f"创建用户 {name}", st in (200, 201), f"{st} {body}")
        users[name] = {"id": body.get("user", {}).get("id") or body.get("id"), "username": username}
        st, body = raw("POST", "/api/auth/login",
                       {"username": username, "password": "pass12345"}, with_auth=False)
        check(f"登录 {name}", st == 200, f"{st} {body}")
        USER_TOKENS[name] = body.get("access_token", "")

    st, body = api("POST", "/api/admin/groups",
                   {"name": f"e2e-pm-dev-{SUFFIX}", "description": "研发"})
    check("创建分组", st == 201, f"{st} {body}")
    group_id = body.get("group", {}).get("id")
    st, body = api("POST", f"/api/admin/groups/{group_id}/members/add",
                   {"user_ids": [users["alice"]["id"]]})
    check("alice 加入分组", st == 200 and body.get("added") == 1, f"{st} {body}")

    u = "/api/admin/plans"
    pu = f"{u}/{plan_id}"

    # ---------- 1. 正常添加 ----------
    print("== 正常添加 ==")
    st, body = api("POST", f"{pu}/users/add",
                   {"user_ids": [users["alice"]["id"], users["bob"]["id"]]})
    check("批量添加用户 → added=2", st == 200 and body.get("added") == 2, f"{st} {body}")
    st, body = api("GET", f"{pu}/users")
    names = [m["username"] for m in body.get("members", [])]
    check("成员列表含 alice/bob", st == 200 and body.get("total") == 2
          and users["alice"]["username"] in names and users["bob"]["username"] in names,
          f"{st} {body}")
    st, body = api("GET", f"{pu}/user-candidates")
    flagged = {r["id"]: r["is_member"] for r in body.get("users", [])}
    check("选择器 is_member 标记", flagged.get(users["alice"]["id"]) is True
          and flagged.get(users["bob"]["id"]) is True
          and flagged.get(users["carol"]["id"]) is False, f"{st} {flagged}")

    # ---------- 2. 搜索 ----------
    print("== 搜索 ==")
    st, body = api("GET", f"{pu}/users", None)
    st, body = api("GET", f"{pu}/users?q=" + users["alice"]["username"][:8])
    check("模糊搜索按用户名命中", st == 200 and body.get("total") == 1, f"{st} {body}")
    st, body = api("GET", f"{pu}/users?q={urllib.parse.quote('不存在的关键字xyz')}")
    check("搜索无命中 → 空列表", st == 200 and body.get("total") == 0
          and body.get("members") == [], f"{st} {body}")
    st, body = api("GET", f"{pu}/user-candidates?q={urllib.parse.quote('不存在的关键字xyz')}")
    check("选择器搜索无命中 → 空", st == 200 and body.get("total") == 0, f"{st} {body}")
    st, body = api("GET", f"{pu}/group-candidates?q=e2e-pm-dev-{SUFFIX}")
    check("组名模糊搜索命中", st == 200 and body.get("total") == 1
          and body["groups"][0]["is_member"] is False, f"{st} {body}")

    print("== 重复添加 ==")
    st, body = api("POST", f"{pu}/users/add", {"user_ids": [users["bob"]["id"]]})
    check("重复添加用户 → 409 conflict", st == 409 and err_code(body) == "conflict", f"{st} {body}")
    check("409 提示含成员名", users["bob"]["username"] in err_msg(body), err_msg(body))
    st, body = api("POST", f"{pu}/users/add",
                   {"user_ids": [users["carol"]["id"], users["bob"]["id"]]})
    check("混合批次重复 → 全量拒绝 409", st == 409 and err_code(body) == "conflict",
          f"{st} {body}")
    st, body = api("GET", f"{pu}/user-candidates")
    carol_member = {r["id"]: r["is_member"] for r in body.get("users", [])}.get(
        users["carol"]["id"])
    check("全有或全无：carol 未被部分写入", carol_member is False, f"{st} {body}")

    # ---------- 分组通道 ----------
    print("== 添加分组 ==")
    st, body = api("POST", f"{pu}/groups/add", {"group_ids": [group_id]})
    check("添加分组 → added=1", st == 200 and body.get("added") == 1, f"{st} {body}")
    st, body = api("POST", f"{pu}/groups/add", {"group_ids": [group_id]})
    check("重复添加分组 → 409", st == 409 and err_code(body) == "conflict", f"{st} {body}")
    st, body = api("GET", f"{pu}/groups")
    check("分组列表含 dev", st == 200 and body.get("total") == 1
          and body["groups"][0]["group_id"] == group_id, f"{st} {body}")
    st, body = api("GET", "/api/admin/plans")
    summary = next((p for p in body.get("plans", []) if p["id"] == plan_id), {})
    check("汇总：group_count=1 member_count=2（alice 双通道去重）",
          summary.get("group_count") == 1 and summary.get("member_count") == 2,
          f"{st} {summary}")

    # ---------- 4. 参数错误 ----------
    print("== 参数错误 ==")
    st, body = api("POST", f"{pu}/users/add", {"user_ids": []})
    check("空 user_ids → 400", st == 400 and err_code(body) == "invalid_request_error",
          f"{st} {body}")
    st, body = api("POST", f"{pu}/groups/add", {"group_ids": []})
    check("空 group_ids → 400", st == 400, f"{st} {body}")
    st, body = api("POST", f"{pu}/users/add", {"user_ids": [999999999]})
    check("不存在的用户 → 400 提示", st == 400 and "不存在" in err_msg(body), f"{st} {body}")
    st, body = api("POST", f"{u}/999999999/users/add", {"user_ids": [1]})
    check("不存在的 Plan → 400", st == 400, f"{st} {body}")

    # ---------- 5. 无权限 ----------
    print("== 无权限 ==")
    st, body = raw("POST", f"{pu}/users/add", {"user_ids": [1]}, with_auth=False)
    check("未登录 → 401 unauthorized", st == 401 and err_code(body) == "unauthorized",
          f"{st} {body}")
    st, body = raw("GET", f"{pu}/users", token=USER_TOKENS["carol"])
    check("普通用户读成员 → 403 forbidden", st == 403 and err_code(body) == "forbidden",
          f"{st} {body}")
    st, body = raw("POST", f"{pu}/users/add",
                   {"user_ids": [users["carol"]["id"]]}, token=USER_TOKENS["carol"])
    check("普通用户添加 → 403", st == 403, f"{st} {body}")

    # ---------- 6. 运行时生效 ----------
    print("== 运行时生效 ==")
    st, body = api("GET", "/api/me/plan", who="bob")
    check("直连用户 bob 命中 Plan", st == 200 and body.get("plan", {})
          and body["plan"].get("name") == plan_name, f"{st} {body}")
    st, body = api("GET", "/api/me/plan", who="alice")
    check("分组+直连用户 alice 命中 Plan", st == 200 and body.get("plan", {})
          and body["plan"].get("name") == plan_name, f"{st} {body}")

    # ---------- 7. 移除 ----------
    print("== 移除成员 ==")
    st, body = api("POST", f"{pu}/users/remove", {"user_ids": [users["bob"]["id"]]})
    check("移除 bob → removed=1", st == 200 and body.get("removed") == 1, f"{st} {body}")
    st, body = api("GET", "/api/me/plan", who="bob")
    check("移除后 bob 不限额", st == 200 and body.get("plan") is None, f"{st} {body}")
    st, body = api("POST", f"{pu}/groups/remove", {"group_ids": [group_id]})
    check("移除分组 → removed=1", st == 200 and body.get("removed") == 1, f"{st} {body}")
    st, body = api("GET", "/api/me/plan", who="alice")
    check("alice 仍走直连通道", st == 200 and body.get("plan", {})
          and body["plan"].get("name") == plan_name, f"{st} {body}")
    st, body = api("POST", f"{pu}/users/remove",
                   {"user_ids": [users["alice"]["id"], users["bob"]["id"]]})
    check("移除不存在的组合 → removed=1（alice）", st == 200 and body.get("removed") == 1,
          f"{st} {body}")
    st, body = api("GET", "/api/me/plan", who="alice")
    check("移除后 alice 不限额", st == 200 and body.get("plan") is None, f"{st} {body}")

    # ---------- 清理 ----------
    print("== 清理 ==")
    st, body = api("DELETE", f"/api/admin/plans/{plan_id}")
    check("删除 Plan 返回影响面", st == 200 and body.get("affected_groups") == 0
          and body.get("affected_direct_users") == 0, f"{st} {body}")

    print(f"\n结果: {PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
