//! Coding Plan 阈值告警派发：站内通知（plan_alerts 行，用户自助可见）+
//! 邮件（SMTP，收件人 = 用户本人 + 全部管理员）+ webhook（Plan 级 URL）。
//!
//! 去重三层：
//! 1. 进程内 seen 集合（热路径短路，同 [`crate::service::usage`] 的 quota_alerts 模式）
//! 2. plan_alerts UNIQUE(plan_id, user_id, period_key, level)（多实例幂等）
//! 3. 派发结果回写 delivered JSONB，便于排障
//!
//! 告警在记账提交后/请求拦截前异步触发，不阻塞代理热路径。

use std::time::Duration;

use crate::state::AppState;
use crate::store::plans::PlanRuntime;

/// 告警级别（已用比例 %）
pub const LEVEL_80: i16 = 80;
pub const LEVEL_95: i16 = 95;
pub const LEVEL_100: i16 = 100;

/// 计算当前应触发的最高告警级别；未达 80% 返回 None
pub fn crossed_level(used: i64, limit: i64) -> Option<i16> {
    if limit <= 0 {
        return None;
    }
    if used >= limit {
        Some(LEVEL_100)
    } else if used * 100 >= 95 * limit {
        Some(LEVEL_95)
    } else if used * 100 >= 80 * limit {
        Some(LEVEL_80)
    } else {
        None
    }
}

/// 阈值命中检查 + 异步派发入口（记账后与请求拦截前共用）
pub fn check_and_dispatch(st: &AppState, user_id: i64, plan: &PlanRuntime, used: i64) {
    let Some(level) = crossed_level(used, plan.token_limit) else {
        return;
    };
    let period_key = plan.period_key(chrono::Utc::now());
    {
        let mut seen = st.plan_alerts_seen.lock();
        if !seen.insert((user_id, plan.plan_id, period_key.clone(), level)) {
            return;
        }
    }
    let st = st.clone();
    let plan = plan.clone();
    tokio::spawn(async move {
        if let Err(e) = dispatch(&st, user_id, &plan, &period_key, level, used).await {
            tracing::warn!(user_id, plan_id = plan.plan_id, level, error = %e, "plan alert dispatch failed");
        }
    });
}

pub fn period_label(period_type: &str) -> &'static str {
    match period_type {
        crate::store::plans::PERIOD_HOURLY => "本时段",
        crate::store::plans::PERIOD_DAILY => "本日",
        crate::store::plans::PERIOD_MONTHLY => "本月",
        _ => "总量",
    }
}

fn build_message(plan: &PlanRuntime, _level: i16, used: i64, username: &str) -> String {
    let pct = if plan.token_limit > 0 {
        used * 100 / plan.token_limit
    } else {
        0
    };
    format!(
        "用户 {username} 的 Coding Plan「{}」{}用量已用 {}/{} tokens（{}%）",
        plan.plan_name,
        period_label(&plan.period_type),
        used,
        plan.token_limit,
        pct
    )
}

/// 落库（幂等）+ 按渠道派发 + 回写派发结果。返回 Err 仅记录日志，不影响主流程。
async fn dispatch(
    st: &AppState,
    user_id: i64,
    plan: &PlanRuntime,
    period_key: &str,
    level: i16,
    used: i64,
) -> Result<(), String> {
    let user = crate::store::users::find_by_id(&st.pool, user_id)
        .await
        .map_err(|e| e.to_string())?
        .map(|u| (u.username.clone(), u.email));
    let username = user.as_ref().map(|(n, _)| n.clone()).unwrap_or_default();
    let message = build_message(plan, level, used, &username);

    // 幂等落库：冲突（他实例已派发）→ 放弃
    let row: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO plan_alerts \
           (plan_id, plan_name, user_id, username, level, period_key, used, limit_tokens, message) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
         ON CONFLICT (plan_id, user_id, period_key, level) DO NOTHING RETURNING id",
    )
    .bind(plan.plan_id)
    .bind(&plan.plan_name)
    .bind(user_id)
    .bind(&username)
    .bind(level)
    .bind(period_key)
    .bind(used)
    .bind(plan.token_limit)
    .bind(&message)
    .fetch_optional(&st.pool)
    .await
    .map_err(|e| e.to_string())?;
    if row.is_none() {
        return Ok(());
    }
    let mut delivered = serde_json::Map::new();
    delivered.insert("in_site".into(), serde_json::json!("ok"));

    // 邮件：用户本人 + 全部启用管理员（有邮箱者）
    if plan.alert_channels.iter().any(|c| c == "email") {
        let email_result = match send_plan_email(
            st,
            plan,
            &message,
            user.as_ref().and_then(|(_, e)| e.clone()),
        )
        .await
        {
            Ok(n) => serde_json::json!(format!("ok: {n} recipients")),
            Err(e) => serde_json::json!(format!("error: {e}")),
        };
        delivered.insert("email".into(), email_result);
    }

    // webhook：Plan 级 URL
    if plan.alert_channels.iter().any(|c| c == "webhook") && !plan.webhook_url.trim().is_empty() {
        let payload = serde_json::json!({
            "type": "plan.alert",
            "level": level,
            "plan": {"id": plan.plan_id, "name": plan.plan_name},
            "user": {"id": user_id, "username": username},
            "period": period_key,
            "used": used,
            "limit": plan.token_limit,
            "message": message,
        });
        let webhook_result = match st
            .client
            .post(plan.webhook_url.trim())
            .timeout(Duration::from_secs(10))
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) => serde_json::json!(format!("http_{}", resp.status().as_u16())),
            Err(e) => serde_json::json!(format!("error: {e}")),
        };
        delivered.insert("webhook".into(), webhook_result);
    }

    // 派发结果回写（best-effort）
    let _ = sqlx::query("UPDATE plan_alerts SET delivered = $1 WHERE id = $2")
        .bind(serde_json::Value::Object(delivered))
        .bind(row.map(|(id,)| id))
        .execute(&st.pool)
        .await;
    Ok(())
}

/// 告警邮件：收件人 = 用户邮箱 + 全部管理员邮箱。SMTP 未配置 → 跳过（记 ok: skipped）。
async fn send_plan_email(
    st: &AppState,
    plan: &PlanRuntime,
    message: &str,
    user_email: Option<String>,
) -> Result<String, String> {
    let smtp = crate::store::config::load_smtp_settings(&st.pool)
        .await
        .map_err(|e| e.to_string())?;
    if !smtp.configured() {
        return Ok("skipped: SMTP not configured".into());
    }
    let mut tos: Vec<String> = Vec::new();
    if let Some(e) = user_email.filter(|e| !e.trim().is_empty()) {
        tos.push(e);
    }
    let admins: Vec<(String,)> = sqlx::query_as(
        "SELECT email FROM users WHERE is_admin = TRUE AND status = 1 \
          AND email IS NOT NULL AND email <> ''",
    )
    .fetch_all(&st.pool)
    .await
    .map_err(|e| e.to_string())?;
    for (e,) in admins {
        if !tos.contains(&e) {
            tos.push(e);
        }
    }
    if tos.is_empty() {
        return Ok("skipped: no email recipients".into());
    }
    let subject = format!("[LLM Gateway] Coding Plan「{}」用量告警", plan.plan_name);
    send_email(&st.cfg.master_key, &smtp, &subject, message, &tos).await
}

/// SMTP 测试发信（管理端配置验证）
pub async fn send_test_email(
    st: &AppState,
    smtp: &crate::store::config::SmtpSettings,
    to: &str,
) -> Result<(), String> {
    send_email(
        &st.cfg.master_key,
        smtp,
        "[LLM Gateway] SMTP 配置测试",
        "这是一封测试邮件：Coding Plan 告警邮件通道配置有效。",
        std::slice::from_ref(&to.to_string()),
    )
    .await
    .map(|_| ())
}

/// SMTP 发信（lettre，rustls）。465 隐式 TLS，其余端口 STARTTLS。
async fn send_email(
    master_key: &[u8; 32],
    smtp: &crate::store::config::SmtpSettings,
    subject: &str,
    body: &str,
    tos: &[String],
) -> Result<String, String> {
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let from: lettre::message::Mailbox = smtp
        .from
        .trim()
        .parse()
        .map_err(|_| format!("invalid smtp from: {:?}", smtp.from))?;

    let builder = if smtp.port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host)
    }
    .map_err(|e| format!("smtp tls setup failed: {e}"))?
    .port(smtp.port);
    let builder = if smtp.username.trim().is_empty() {
        builder
    } else {
        let password = crate::crypto::decrypt(&smtp.password_enc, master_key)
            .map_err(|e| format!("smtp password decrypt failed: {e}"))?;
        builder.credentials(Credentials::new(smtp.username.clone(), password))
    };
    let mailer = builder.build();

    let mut sent = 0usize;
    for to in tos {
        let addr: lettre::message::Mailbox = to
            .trim()
            .parse()
            .map_err(|_| format!("invalid recipient email: {to:?}"))?;
        let email = Message::builder()
            .from(from.clone())
            .to(addr)
            .subject(subject)
            .body(body.to_string())
            .map_err(|e| format!("build email failed: {e}"))?;
        mailer
            .send(email)
            .await
            .map_err(|e| format!("smtp send failed: {e}"))?;
        sent += 1;
    }
    Ok(format!("{sent}"))
}

/// 系统兜底通知（level=0，plan_id/user_id 为 NULL）：看板最近事件可见。
/// 用于 Plan 删除/停用导致分组失去绑定等运维事件。
pub async fn system_notice(st: &AppState, message: &str) {
    let res = sqlx::query(
        "INSERT INTO plan_alerts (plan_id, plan_name, user_id, username, level, message) \
         VALUES (NULL, '', NULL, '', 0, $1)",
    )
    .bind(message)
    .execute(&st.pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "system notice insert failed");
    }
}
