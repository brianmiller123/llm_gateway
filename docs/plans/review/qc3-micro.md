---
report_kind: qc
reviewer: qc-specialist-3
reviewer_index: 3
plan_id: "N/A — micro-delta re-review"
verdict: "Approve"
generated_at: "2026-11-12"
---

# QC3 Micro Review — single-source SSE tail/settlement predicate

**Review range:** `git diff a70d09f^..a70d09f` (`src/service/proxy.rs`, +18/−29), plus merge `0ee7b03` (parents `6a38814`, `a70d09f`).
**Worktree:** `/home/pop/AI/code/llm_gateway.wt/integration` (branch `fix/long-task-admin`, HEAD `0ee7b03`).
**Basis:** read-only, diff + read + grep. No build/test/install/lint executed.
**Focus:** performance and reliability risk; this round is scoped to the settlement-predicate single-sourcing delta.

## Scope

- Files opened in repo: `src/service/proxy.rs` (diff + ranges 1336–1420, 1490–1603, 1690–1800) — 1 file.
- Verification commands: `git diff/show/log`, `grep` over `src/`.
- Truncated coverage: none — full delta and its directly affected install point reviewed.

## Verification of the four assigned questions

### 1. Single computation, shared value; second equivalent predicate

- `tail_settles` is declared exactly once (`proxy.rs:1357`), **immutably** (`let`, no `mut`), inside the same per-candidate loop-body scope that contains both consumers. Rust's borrow checker makes post-declaration reassignment/rebinding impossible; grep confirms only three textual sites: declaration (1357), commit settlement (1541), install point (1737).
- Commit settlement and `SseErrorContext` installation consume the *same* value; `settle_commit_signal` no longer re-derives it (signature reduced to `bool`, internal `contains` removed).
- `is_sse_body` is **fully removed** — `grep -rn 'is_sse_body' src/` returns none. No residual second settlement predicate exists.
- Remaining `content_type.contains("text/event-stream")` occurrences are **not** equivalent predicates and are correctly left alone: `:1557`/`:1621` gate conversion-chain construction (conjunct with `status.is_success()` and `anthropic_converted`/`converted`), `:1776` gates the `Cache-Control: no-cache` header. None feeds breaker settlement or tail installation.
- Test helper `drive_committed_sse` updated to `settle_commit_signal(breaker, 7, true)` — semantically identical to the previous `(false, false, "text/event-stream")` call (same computed predicate), so no test-coverage semantics changed.

### 2. "Conversion chains return earlier" invariant still holds

Traced every branch between commit (`:1541`) and the installation point (`:1737`):

- `anthropic_converted && success && is_sse` (`:1561`) → `return`.
- `anthropic_converted` (`:1591`) → `return` (covers non-2xx and 200-non-SSE).
- `converted && success && is_sse` (`:1629`) → `return`.
- `converted && success` (`:1652`) → both arms `return`.
- `converted` (`:1700`) → `return`.
- The only non-returning pre-install exit narrowing the stream is the priming `None` JSON-passthrough branch, which requires `!is_sse` and therefore also yields `tail_settles == false` under either the old or the new predicate.

Consequently the install point is reachable **only** with `!anthropic_converted && !converted`, so `tail_settles == is_sse` there — exactly the old `is_sse_body` value. Even under a hypothetical fall-through from a `converted` branch, `content_type` would not be SSE and both old and new predicates would be `false`; no divergence path exists.

Lifecycle: `tail_settles` is per-iteration and never survives a `continue 'candidates` iteration; there is no assignment that could change it after the commit use.

### 3. Ping/keepalive branch: `is_sse` vs old `is_sse_body`

Identical expression over the same value. `content_type` is bound once at `:1331` (shadowing the outer `String`) and is never reassigned; all later occurrences are reads or uses inside early-returning branches (the move at `:1638` is inside a branch that returns). Therefore `is_sse` (`:1353`, computed pre-priming) and the removed `is_sse_body` (computed post-priming) are provably the same boolean on every path that reaches `:1762`. Semantics of the keepalive decision are unchanged.

### 4. Allocation / lock behavior

- `tail_settles: bool` is stack-only, copied by value; no heap allocation added.
- One `content_type.contains(..)` call and one `&str` argument are **removed** from the commit path (strictly fewer operations).
- `SseErrorContext::then(..)` remains lazy construction over a `bool` — no change in how many contexts are built.
- No lock, channel, task spawn, or clone added or removed; no new `await` points.

## Findings

findings: []

No Critical, Warning, or Nit-level defect attributable to this delta. Two informational observations, explicitly **not** findings because they carry zero behavioral or drift risk:

- `:1776` still spells `content_type.contains("text/event-stream")` although `is_sse` is in scope; value is provably identical (immutable `content_type`), so this is a transparency nit only, not a duplicated decision that can drift.
- `tail_settles` is computed before the priming phase even for candidates that fail over before commit; cost is one bool, no allocation, and per-iteration recomputation is the correct scope.

## Merge check

`0ee7b03` is a two-parent merge (`6a38814`, `a70d09f`); `git diff --quiet a70d09f 0ee7b03 -- src/service/proxy.rs` reports IDENTICAL, so the merge introduced no change to the reviewed file. The `src/api/console.rs` divergence visible in the merge tree belongs to the sibling admin-source work, outside this delta's scope.

## Verdict

**Approve** — the settlement decision is genuinely single-sourced, the conversion-chain early-return invariant is intact, the keepalive predicate is semantically identical to the removed one, and the change adds no allocation or locking.
