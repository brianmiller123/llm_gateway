use crate::state::AppState;
use crate::store::upstream::ModelRoute;

/// 路由匹配：精确命中优先；无精确命中时按 priority 升序取首个通配命中（routes 已按 priority,id 排序）
pub fn resolve_route<'a>(routes: &'a [ModelRoute], model: &str) -> Option<&'a ModelRoute> {
    routes
        .iter()
        .find(|r| r.model_pattern == model)
        .or_else(|| {
            routes
                .iter()
                .find(|r| matches_pattern(&r.model_pattern, model))
        })
}

/// 通配匹配：仅支持尾缀 `*`（与路由规则同语义）；精确相等也命中
pub fn matches_pattern(pattern: &str, model: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        model.starts_with(prefix)
    } else {
        pattern == model
    }
}

/// 模型库启停：该供应商下的（上游侧）模型是否已被管理员禁用。
/// 代理管线按出站模型名剔除候选，/v1/models 按目录行过滤——同一判定。
pub fn model_disabled(st: &AppState, provider_id: i64, upstream_model: &str) -> bool {
    st.disabled_models
        .read()
        .get(&provider_id)
        .is_some_and(|models| models.contains(upstream_model))
}

/// 用户访问授权判定（白名单）：
/// - admin 用户不受限
/// - 用户无任何规则 = 默认放行（兼容既有账号）
/// - 有规则：命中任一 (provider_id IS NULL=任意, model_pattern IS NULL=全部模型) 组合即放行
pub fn user_can_use(st: &AppState, user_id: i64, provider_id: i64, model: &str) -> bool {
    let access = st.user_access.read();
    let Some(rules) = access.get(&user_id) else {
        return true;
    };
    if rules.is_empty() {
        return true;
    }
    rules.iter().any(|r| {
        (r.provider_id.is_none() || r.provider_id == Some(provider_id))
            && (r.model_pattern.is_none()
                || r.model_pattern
                    .as_deref()
                    .is_some_and(|p| matches_pattern(p, model)))
    })
}

/// 计算计费金额：cost = in/1e6 * p_in + out/1e6 * p_out + 缓存桶独立费率
/// （未配置缓存单价时回退 input 单价，与历史行为一致）；无单价记 0。
/// 注意 input_tokens 已包含缓存桶（OpenAI 口径），缓存部分按缓存费率、
/// 其余 (input − cache_read − cache_write) 按 input 费率，不重复计费。
/// L4：内部按微美元整数累计（i128），终值才转 f64——消除逐桶 f64 累加的
/// 舍入漂移（cc-switch usage/calculator.rs Decimal 精确计费同款动机，
/// 不引入新依赖）。
pub fn compute_cost(
    st: &AppState,
    model: &str,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
) -> f64 {
    let prices = st.prices.read();
    let Some(price) = prices.get(model) else {
        return 0.0;
    };
    // 单价（每百万 token）→ 微美元整数（每百万 token 的价格 × 1e6）
    let micros = |p: f64| (p * 1_000_000.0).round() as i128;
    let bucket =
        |tokens: i64, p_micros: i128| (tokens as i128).saturating_mul(p_micros) / 1_000_000;
    let mut cost_micros: i128 = 0;
    if let Some(t) = input_tokens {
        let cache_read = cache_read_tokens.unwrap_or(0).min(t);
        let cache_write = cache_write_tokens
            .unwrap_or(0)
            .min(t.saturating_sub(cache_read));
        let fresh = t - cache_read - cache_write;
        if let Some(p) = price.input_price_per_m {
            cost_micros += bucket(fresh, micros(p));
        }
        if cache_read > 0 {
            if let Some(p) = price.cache_read_price_per_m.or(price.input_price_per_m) {
                cost_micros += bucket(cache_read, micros(p));
            }
        }
        if cache_write > 0 {
            if let Some(p) = price.cache_write_price_per_m.or(price.input_price_per_m) {
                cost_micros += bucket(cache_write, micros(p));
            }
        }
    }
    if let (Some(t), Some(p)) = (output_tokens, price.output_price_per_m) {
        cost_micros += bucket(t, micros(p));
    }
    cost_micros as f64 / 1_000_000.0
}
