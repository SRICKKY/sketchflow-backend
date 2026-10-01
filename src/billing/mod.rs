use chrono::{DateTime, Datelike, Utc};
use sqlx::PgPool;

use crate::{config::AppConfig, error::AppError};

pub mod razorpay;

pub const RAZORPAY_MIN_AMOUNT: i64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BillingInterval {
    Month,
    Year,
}

impl BillingInterval {
    pub fn as_str(self) -> &'static str {
        match self {
            BillingInterval::Month => "month",
            BillingInterval::Year => "year",
        }
    }

    pub fn parse(value: &str) -> Self {
        if value == "year" {
            BillingInterval::Year
        } else {
            BillingInterval::Month
        }
    }

    pub fn parse_opt(value: Option<&str>) -> Self {
        value.map(Self::parse).unwrap_or(BillingInterval::Month)
    }
}

/// Test-mode INR paise amounts, mirroring the hardcoded values in the
/// Next.js `checkoutAmountMinor` helper.
pub fn checkout_amount_minor(config: &AppConfig, interval: BillingInterval) -> i64 {
    match interval {
        BillingInterval::Month => config.plan_price_month_minor,
        BillingInterval::Year => config.plan_price_year_minor,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingStatus {
    Free,
    Active,
    Expired,
}

pub struct ResolvedBilling {
    pub status: BillingStatus,
    pub plan: &'static str,
    pub interval: Option<BillingInterval>,
    pub expires_at: Option<DateTime<Utc>>,
}

pub fn add_billing_period(from: DateTime<Utc>, interval: BillingInterval) -> DateTime<Utc> {
    match interval {
        BillingInterval::Year => shift_years(from, 1),
        BillingInterval::Month => shift_months(from, 1),
    }
}

pub fn subtract_billing_period(from: DateTime<Utc>, interval: BillingInterval) -> DateTime<Utc> {
    match interval {
        BillingInterval::Year => shift_years(from, -1),
        BillingInterval::Month => shift_months(from, -1),
    }
}

fn shift_years(from: DateTime<Utc>, delta: i32) -> DateTime<Utc> {
    from.with_year(from.year() + delta).unwrap_or(from)
}

fn shift_months(from: DateTime<Utc>, delta: i32) -> DateTime<Utc> {
    let total = from.month() as i32 - 1 + delta;
    let year = from.year() + total.div_euclid(12);
    let month = total.rem_euclid(12) + 1;
    from.with_year(year)
        .and_then(|d| d.with_month(month as u32))
        .unwrap_or(from)
}

/// Prefers a live active subscription row; falls back to the user's
/// `plan`/`plan_expires_at` columns. Mirrors `resolveBillingState`.
pub fn resolve_billing_state(
    active_sub_interval: Option<&str>,
    active_sub_expires_at: Option<DateTime<Utc>>,
    user_plan: Option<&str>,
    user_expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> ResolvedBilling {
    let sub_live = active_sub_expires_at.is_some_and(|e| e > now);
    let user_live = user_expires_at.is_some_and(|e| e > now);

    if sub_live {
        return ResolvedBilling {
            status: BillingStatus::Active,
            plan: "pro",
            interval: Some(BillingInterval::parse_opt(active_sub_interval)),
            expires_at: active_sub_expires_at,
        };
    }

    if user_plan == Some("pro") && user_live {
        return ResolvedBilling {
            status: BillingStatus::Active,
            plan: "pro",
            interval: Some(BillingInterval::parse_opt(active_sub_interval)),
            expires_at: user_expires_at,
        };
    }

    let expired_at = active_sub_expires_at.or(user_expires_at);
    if let Some(expired_at) = expired_at {
        if expired_at <= now {
            return ResolvedBilling {
                status: BillingStatus::Expired,
                plan: "free",
                interval: active_sub_interval.map(BillingInterval::parse),
                expires_at: Some(expired_at),
            };
        }
    }

    ResolvedBilling {
        status: BillingStatus::Free,
        plan: "free",
        interval: None,
        expires_at: user_expires_at,
    }
}

pub struct BillingState {
    pub status: BillingStatus,
    pub plan: &'static str,
    pub interval: Option<BillingInterval>,
    pub starts_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct ActiveSubscriptionRow {
    interval: String,
    starts_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct UserPlanRow {
    plan: String,
    plan_expires_at: Option<DateTime<Utc>>,
}

/// Looks up the user's current billing state, expiring overdue subscription
/// rows and syncing the denormalized `users.plan` column as a side effect
/// (mirrors `getBillingState`).
pub async fn get_billing_state(db: &PgPool, user_id: &str) -> Result<BillingState, AppError> {
    let now = Utc::now();

    let latest = sqlx::query_as::<_, ActiveSubscriptionRow>(
        r#"SELECT interval, starts_at, expires_at
           FROM subscriptions
           WHERE user_id = $1 AND status = 'active'
           ORDER BY expires_at DESC
           LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;

    let user = sqlx::query_as::<_, UserPlanRow>(
        r#"SELECT plan, plan_expires_at FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::Unauthorized("Sign in to continue.".to_string()))?;

    let resolved = resolve_billing_state(
        latest.as_ref().map(|s| s.interval.as_str()),
        latest.as_ref().map(|s| s.expires_at),
        Some(user.plan.as_str()),
        user.plan_expires_at,
        now,
    );

    if latest.as_ref().is_some_and(|s| s.expires_at <= now) {
        sqlx::query(
            r#"UPDATE subscriptions SET status = 'expired', updated_at = now()
               WHERE user_id = $1 AND status = 'active' AND expires_at <= $2"#,
        )
        .bind(user_id)
        .bind(now)
        .execute(db)
        .await?;
    }

    let starts_at = latest.as_ref().map(|s| s.starts_at).or_else(|| {
        resolved
            .expires_at
            .zip(resolved.interval)
            .map(|(expires_at, interval)| subtract_billing_period(expires_at, interval))
    });

    let same_expiry = user.plan_expires_at.map(|e| e.timestamp())
        == resolved
            .expires_at
            .map(|e| e.timestamp())
            .or(user.plan_expires_at.map(|e| e.timestamp()));
    if resolved.plan == "pro" && (user.plan != "pro" || !same_expiry) {
        sqlx::query(
            r#"UPDATE users SET plan = 'pro', plan_expires_at = $2, updated_at = now() WHERE id = $1"#,
        )
        .bind(user_id)
        .bind(resolved.expires_at)
        .execute(db)
        .await?;
    } else if resolved.plan == "free" && user.plan == "pro" {
        let expires_at = resolved.expires_at.or(user.plan_expires_at);
        sqlx::query(
            r#"UPDATE users SET plan = 'free', plan_expires_at = $2, updated_at = now() WHERE id = $1"#,
        )
        .bind(user_id)
        .bind(expires_at)
        .execute(db)
        .await?;
    }

    Ok(BillingState {
        status: resolved.status,
        plan: resolved.plan,
        interval: resolved.interval,
        starts_at,
        expires_at: resolved.expires_at,
    })
}

pub struct CheckoutQuote {
    pub blocked: bool,
    pub blocked_message: Option<String>,
    pub interval: BillingInterval,
    pub amount_minor: i64,
}

fn unused_period_credit_minor(
    paid_minor: i64,
    starts_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> i64 {
    let period = (expires_at - starts_at).num_milliseconds();
    if period <= 0 || now >= expires_at {
        return 0;
    }
    let remaining = (expires_at - now).num_milliseconds().clamp(0, period);
    ((paid_minor as i128 * remaining as i128) / period as i128) as i64
}

/// Mirrors `quoteCheckout`: computes the amount to charge for the requested
/// interval, applying prorated credit when switching monthly -> annual.
pub fn quote_checkout(
    config: &AppConfig,
    interval: BillingInterval,
    billing: &BillingState,
    now: DateTime<Utc>,
) -> CheckoutQuote {
    let full = checkout_amount_minor(config, interval);
    if billing.status != BillingStatus::Active {
        return CheckoutQuote {
            blocked: false,
            blocked_message: None,
            interval,
            amount_minor: full,
        };
    }

    let current = billing.interval.unwrap_or(BillingInterval::Month);
    if interval == BillingInterval::Year && current == BillingInterval::Month {
        let expires_at = billing.expires_at;
        let starts_at = billing
            .starts_at
            .or_else(|| expires_at.map(|e| subtract_billing_period(e, BillingInterval::Month)));
        let credit = match (starts_at, expires_at) {
            (Some(s), Some(e)) => unused_period_credit_minor(
                checkout_amount_minor(config, BillingInterval::Month),
                s,
                e,
                now,
            ),
            _ => 0,
        };
        let year_full = checkout_amount_minor(config, BillingInterval::Year);
        return CheckoutQuote {
            blocked: false,
            blocked_message: None,
            interval: BillingInterval::Year,
            amount_minor: (year_full - credit).max(RAZORPAY_MIN_AMOUNT),
        };
    }

    CheckoutQuote {
        blocked: true,
        blocked_message: Some(if current == BillingInterval::Year {
            "Annual Pro is already active. Monthly billing can start when this year ends."
                .to_string()
        } else {
            "Monthly Pro is already active.".to_string()
        }),
        interval,
        amount_minor: 0,
    }
}

fn invoice_number(paid_at: DateTime<Utc>, unique: &str) -> String {
    let suffix: String = unique
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>()
        .to_uppercase();
    format!(
        "INV-{:04}{:02}{:02}-{}",
        paid_at.year(),
        paid_at.month(),
        paid_at.day(),
        if suffix.is_empty() { "PAID" } else { &suffix }
    )
}

pub struct ActivatedSubscription {
    pub interval: BillingInterval,
    pub expires_at: DateTime<Utc>,
}

/// Idempotently activates (or re-confirms) a Pro subscription for a
/// successful Razorpay payment, mirroring `activateProSubscription`.
pub async fn activate_pro_subscription(
    db: &PgPool,
    user_id: &str,
    interval: BillingInterval,
    payment_id: &str,
    gateway_order_id: &str,
    gateway_payment_id: &str,
    amount_minor: i64,
    currency: &str,
) -> Result<ActivatedSubscription, AppError> {
    #[derive(sqlx::FromRow)]
    struct ExistingRow {
        interval: String,
        expires_at: DateTime<Utc>,
    }
    let existing = sqlx::query_as::<_, ExistingRow>(
        r#"SELECT interval, expires_at FROM subscriptions WHERE payment_id = $1"#,
    )
    .bind(payment_id)
    .fetch_optional(db)
    .await?;

    if let Some(existing) = existing {
        return Ok(ActivatedSubscription {
            interval: BillingInterval::parse(&existing.interval),
            expires_at: existing.expires_at,
        });
    }

    let starts_at = Utc::now();
    let expires_at = add_billing_period(starts_at, interval);
    let currency = {
        let trimmed = currency.trim().to_uppercase();
        if trimmed.is_empty() {
            "INR".to_string()
        } else {
            trimmed
        }
    };
    let amount_minor = if amount_minor > 0 {
        amount_minor
    } else {
        50_000
    };

    let mut tx = db.begin().await?;

    sqlx::query(
        r#"UPDATE subscriptions SET status = 'superseded', updated_at = now()
           WHERE user_id = $1 AND status = 'active'"#,
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;

    let subscription_id = cuid2::create_id();
    sqlx::query(
        r#"INSERT INTO subscriptions
           (id, user_id, interval, status, starts_at, expires_at, payment_id, gateway_order_id, gateway_payment_id)
           VALUES ($1, $2, $3, 'active', $4, $5, $6, $7, $8)"#,
    )
    .bind(&subscription_id)
    .bind(user_id)
    .bind(interval.as_str())
    .bind(starts_at)
    .bind(expires_at)
    .bind(payment_id)
    .bind(gateway_order_id)
    .bind(gateway_payment_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"UPDATE users SET plan = 'pro', plan_expires_at = $2, updated_at = now() WHERE id = $1"#,
    )
    .bind(user_id)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;

    let invoice_id = cuid2::create_id();
    sqlx::query(
        r#"INSERT INTO invoices
           (id, user_id, subscription_id, number, interval, amount_minor, currency, status, paid_at, period_start, period_end, gateway_order_id, gateway_payment_id)
           VALUES ($1, $2, $3, $4, $5, $6, $7, 'paid', $8, $9, $10, $11, $12)"#,
    )
    .bind(&invoice_id)
    .bind(user_id)
    .bind(&subscription_id)
    .bind(invoice_number(starts_at, &subscription_id))
    .bind(interval.as_str())
    .bind(amount_minor)
    .bind(&currency)
    .bind(starts_at)
    .bind(starts_at)
    .bind(expires_at)
    .bind(gateway_order_id)
    .bind(gateway_payment_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(ActivatedSubscription {
        interval,
        expires_at,
    })
}
