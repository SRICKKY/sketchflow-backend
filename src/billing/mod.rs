use chrono::{DateTime, Datelike, TimeZone, Utc};
use sqlx::PgPool;

use crate::{config::AppConfig, error::AppError};

pub mod invoice_pdf;
pub mod limits;
pub mod paddle;
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
    pub lifetime_pro: bool,
    pub paid_amount_minor: Option<i64>,
    pub starts_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct ActiveSubscriptionRow {
    id: String,
    interval: String,
    starts_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct PaidInvoiceRow {
    amount_minor: i32,
    currency: String,
}

#[derive(sqlx::FromRow)]
struct UserPlanRow {
    plan: String,
    plan_expires_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct ActiveLifetimePurchaseRow {
    starts_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

pub fn lifetime_expiry() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0)
        .single()
        .expect("2100-01-01 is a valid UTC timestamp")
}

/// Looks up the user's current billing state, expiring overdue subscription
/// rows and syncing the denormalized `users.plan` column as a side effect
/// (mirrors `getBillingState`).
pub async fn get_billing_state(db: &PgPool, user_id: &str) -> Result<BillingState, AppError> {
    let now = Utc::now();

    let latest = sqlx::query_as::<_, ActiveSubscriptionRow>(
        r#"SELECT id, interval, starts_at, expires_at
           FROM subscriptions
           WHERE user_id = $1 AND status = 'active'
           ORDER BY expires_at DESC
           LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;

    let paid_amount_minor = if let Some(subscription) = &latest {
        let invoice = sqlx::query_as::<_, PaidInvoiceRow>(
            r#"SELECT amount_minor, currency
           FROM invoices
           WHERE subscription_id = $1 AND status = 'paid'
           ORDER BY paid_at DESC
           LIMIT 1"#,
        )
        .bind(&subscription.id)
        .fetch_optional(db)
        .await?;
        invoice.and_then(|invoice| {
            let amount = i64::from(invoice.amount_minor);
            match invoice.currency.trim().to_uppercase().as_str() {
                "INR" if amount > 0 => Some(amount),
                "USD" if amount > 0 => amount.checked_mul(100),
                _ => None,
            }
        })
    } else {
        None
    };

    let user = sqlx::query_as::<_, UserPlanRow>(
        r#"SELECT plan, plan_expires_at FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::Unauthorized("Sign in to continue.".to_string()))?;

    sqlx::query(
        r#"UPDATE lifetime_purchases SET status = 'expired', updated_at = now()
           WHERE user_id = $1 AND status = 'active' AND expires_at <= $2"#,
    )
    .bind(user_id)
    .bind(now)
    .execute(db)
    .await?;

    let lifetime = sqlx::query_as::<_, ActiveLifetimePurchaseRow>(
        r#"SELECT starts_at, expires_at
           FROM lifetime_purchases
           WHERE user_id = $1 AND status = 'active' AND expires_at > $2
           ORDER BY expires_at DESC
           LIMIT 1"#,
    )
    .bind(user_id)
    .bind(now)
    .fetch_optional(db)
    .await?;

    if let Some(lifetime) = lifetime {
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
        if user.plan != "pro"
            || user.plan_expires_at.map(|e| e.timestamp()) != Some(lifetime.expires_at.timestamp())
        {
            sqlx::query(
                r#"UPDATE users SET plan = 'pro', plan_expires_at = $2, updated_at = now()
                   WHERE id = $1"#,
            )
            .bind(user_id)
            .bind(lifetime.expires_at)
            .execute(db)
            .await?;
        }
        return Ok(BillingState {
            status: BillingStatus::Active,
            plan: "pro",
            interval: None,
            lifetime_pro: true,
            paid_amount_minor: None,
            starts_at: Some(lifetime.starts_at),
            expires_at: Some(lifetime.expires_at),
        });
    }

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
        lifetime_pro: false,
        paid_amount_minor,
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

pub struct LifetimeCheckoutQuote {
    pub blocked: bool,
    pub blocked_message: Option<String>,
    pub amount_minor: i64,
    pub credit_minor: i64,
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

pub fn quote_lifetime_checkout(
    config: &AppConfig,
    billing: &BillingState,
    now: DateTime<Utc>,
) -> LifetimeCheckoutQuote {
    let full = config.plan_price_lifetime_minor;
    if billing.lifetime_pro {
        return LifetimeCheckoutQuote {
            blocked: true,
            blocked_message: Some("Lifetime Pro is already active.".to_string()),
            amount_minor: 0,
            credit_minor: 0,
        };
    }
    if full < RAZORPAY_MIN_AMOUNT {
        return LifetimeCheckoutQuote {
            blocked: true,
            blocked_message: Some("Lifetime Pro pricing is invalid.".to_string()),
            amount_minor: 0,
            credit_minor: 0,
        };
    }

    let paid_price = if billing.status == BillingStatus::Active {
        billing.interval.map(|interval| {
            billing
                .paid_amount_minor
                .unwrap_or_else(|| checkout_amount_minor(config, interval))
        })
    } else {
        None
    };
    let credit = match (paid_price, billing.starts_at, billing.expires_at) {
        (Some(paid), Some(starts_at), Some(expires_at)) => {
            unused_period_credit_minor(paid, starts_at, expires_at, now)
        }
        _ => 0,
    };
    let credit_minor = credit.max(0).min(full - RAZORPAY_MIN_AMOUNT);
    LifetimeCheckoutQuote {
        blocked: false,
        blocked_message: None,
        amount_minor: full - credit_minor,
        credit_minor,
    }
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

pub struct ActivatedLifetimePurchase {
    pub expires_at: DateTime<Utc>,
}

pub async fn activate_lifetime_purchase(
    db: &PgPool,
    user_id: &str,
    gateway: &str,
    gateway_order_id: &str,
    gateway_payment_id: &str,
    amount_minor: i64,
    credit_minor: i64,
    currency: &str,
) -> Result<ActivatedLifetimePurchase, AppError> {
    #[derive(sqlx::FromRow)]
    struct ExistingPurchase {
        user_id: String,
        expires_at: DateTime<Utc>,
    }

    let existing = sqlx::query_as::<_, ExistingPurchase>(
        r#"SELECT user_id, expires_at FROM lifetime_purchases
           WHERE gateway_payment_id = $1"#,
    )
    .bind(gateway_payment_id)
    .fetch_optional(db)
    .await?;
    if let Some(existing) = existing {
        if existing.user_id != user_id {
            return Err(AppError::Forbidden(
                "Lifetime purchase belongs to a different user.".to_string(),
            ));
        }
        return Ok(ActivatedLifetimePurchase {
            expires_at: existing.expires_at,
        });
    }

    let amount_minor = i32::try_from(amount_minor)
        .ok()
        .filter(|amount| *amount > 0)
        .ok_or_else(|| AppError::BadRequest("Lifetime purchase amount is invalid.".into()))?;
    let credit_minor = i32::try_from(credit_minor)
        .ok()
        .filter(|credit| *credit >= 0)
        .ok_or_else(|| AppError::BadRequest("Lifetime credit is invalid.".into()))?;
    let currency = currency.trim().to_uppercase();
    if currency.is_empty() {
        return Err(AppError::BadRequest(
            "Lifetime purchase currency is invalid.".into(),
        ));
    }

    let starts_at = Utc::now();
    let expires_at = lifetime_expiry();
    let coverage_end = expires_at - chrono::Duration::milliseconds(1);
    let mut tx = db.begin().await?;
    let purchase_id = cuid2::create_id();
    let inserted = sqlx::query_scalar::<_, String>(
        r#"INSERT INTO lifetime_purchases
           (id, user_id, status, amount_minor, credit_minor, currency, gateway,
            gateway_order_id, gateway_payment_id, starts_at, expires_at)
           VALUES ($1, $2, 'active', $3, $4, $5, $6, $7, $8, $9, $10)
           ON CONFLICT (gateway_payment_id) DO NOTHING
           RETURNING id"#,
    )
    .bind(&purchase_id)
    .bind(user_id)
    .bind(amount_minor)
    .bind(credit_minor)
    .bind(&currency)
    .bind(gateway)
    .bind(gateway_order_id)
    .bind(gateway_payment_id)
    .bind(starts_at)
    .bind(expires_at)
    .fetch_optional(&mut *tx)
    .await?;

    if inserted.is_none() {
        let existing = sqlx::query_as::<_, ExistingPurchase>(
            r#"SELECT user_id, expires_at FROM lifetime_purchases
               WHERE gateway_payment_id = $1"#,
        )
        .bind(gateway_payment_id)
        .fetch_one(&mut *tx)
        .await?;
        if existing.user_id != user_id {
            return Err(AppError::Forbidden(
                "Lifetime purchase belongs to a different user.".to_string(),
            ));
        }
        tx.commit().await?;
        return Ok(ActivatedLifetimePurchase {
            expires_at: existing.expires_at,
        });
    }

    sqlx::query(
        r#"UPDATE users SET plan = 'pro', plan_expires_at = $2, updated_at = now()
           WHERE id = $1"#,
    )
    .bind(user_id)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;

    let invoice_id = cuid2::create_id();
    sqlx::query(
        r#"INSERT INTO invoices
           (id, user_id, subscription_id, number, interval, amount_minor, currency,
            status, paid_at, period_start, period_end, gateway_order_id, gateway_payment_id)
           VALUES ($1, $2, NULL, $3, 'lifetime', $4, $5, 'paid', $6, $6, $7, $8, $9)"#,
    )
    .bind(&invoice_id)
    .bind(user_id)
    .bind(invoice_number(starts_at, &purchase_id))
    .bind(amount_minor)
    .bind(&currency)
    .bind(starts_at)
    .bind(coverage_end)
    .bind(gateway_order_id)
    .bind(gateway_payment_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(ActivatedLifetimePurchase { expires_at })
}

pub async fn revoke_lifetime_purchase(
    db: &PgPool,
    gateway_payment_id: &str,
    status: &str,
) -> Result<bool, AppError> {
    if status != "refunded" && status != "chargeback" {
        return Err(AppError::BadRequest(
            "Lifetime purchase revocation status is invalid.".to_string(),
        ));
    }

    let now = Utc::now();
    let mut tx = db.begin().await?;
    let user_id = sqlx::query_scalar::<_, String>(
        r#"UPDATE lifetime_purchases
           SET status = $2, updated_at = now()
           WHERE gateway_payment_id = $1 AND status = 'active'
           RETURNING user_id"#,
    )
    .bind(gateway_payment_id)
    .bind(status)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user_id) = user_id else {
        tx.commit().await?;
        return Ok(false);
    };

    sqlx::query(
        r#"UPDATE invoices SET status = $2
           WHERE gateway_payment_id = $1"#,
    )
    .bind(gateway_payment_id)
    .bind(status)
    .execute(&mut *tx)
    .await?;
    let active_expiry = sqlx::query_scalar::<_, DateTime<Utc>>(
        r#"SELECT expires_at FROM subscriptions
           WHERE user_id = $1 AND status = 'active' AND expires_at > $2
           ORDER BY expires_at DESC
           LIMIT 1"#,
    )
    .bind(&user_id)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(expires_at) = active_expiry {
        sqlx::query(
            r#"UPDATE users SET plan = 'pro', plan_expires_at = $2, updated_at = now()
               WHERE id = $1"#,
        )
        .bind(&user_id)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            r#"UPDATE users SET plan = 'free', plan_expires_at = NULL, updated_at = now()
               WHERE id = $1"#,
        )
        .bind(&user_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// A single invoice row, scoped to the user that owns it. Mirrors `InvoiceRecord`
/// from `server/billing.ts`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InvoiceRecord {
    pub id: String,
    pub number: String,
    pub interval: String,
    pub amount_minor: i32,
    pub currency: String,
    pub status: String,
    pub paid_at: Option<DateTime<Utc>>,
    pub period_start: Option<DateTime<Utc>>,
    pub period_end: Option<DateTime<Utc>>,
    pub gateway_order_id: Option<String>,
    pub gateway_payment_id: Option<String>,
}

#[derive(sqlx::FromRow)]
struct SubscriptionInvoiceSource {
    id: String,
    interval: String,
    starts_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    payment_id: String,
    gateway_order_id: Option<String>,
    gateway_payment_id: Option<String>,
}

async fn backfill_invoices_for_user(
    db: &PgPool,
    config: &AppConfig,
    user_id: &str,
) -> Result<(), AppError> {
    let subscriptions = sqlx::query_as::<_, SubscriptionInvoiceSource>(
        r#"SELECT id, interval, starts_at, expires_at, payment_id,
                  gateway_order_id, gateway_payment_id
           FROM subscriptions
           WHERE user_id = $1
           ORDER BY starts_at ASC"#,
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;

    for subscription in subscriptions {
        let interval = BillingInterval::parse(&subscription.interval);
        let payment_id = subscription
            .gateway_payment_id
            .as_deref()
            .unwrap_or(&subscription.payment_id);
        let invoice_id = cuid2::create_id();
        let number = invoice_number(subscription.starts_at, &subscription.id);
        sqlx::query(
            r#"INSERT INTO invoices
               (id, user_id, subscription_id, number, interval, amount_minor, currency,
                status, paid_at, period_start, period_end, gateway_order_id, gateway_payment_id)
               SELECT $1, $2, $3, $4, $5, $6, 'INR', 'paid', $7, $7, $8, $9, $10
               WHERE NOT EXISTS (
                   SELECT 1 FROM invoices
                   WHERE subscription_id = $3 OR gateway_payment_id = $10
               )
               ON CONFLICT DO NOTHING"#,
        )
        .bind(invoice_id)
        .bind(user_id)
        .bind(&subscription.id)
        .bind(number)
        .bind(interval.as_str())
        .bind(checkout_amount_minor(config, interval))
        .bind(subscription.starts_at)
        .bind(subscription.expires_at)
        .bind(subscription.gateway_order_id.as_deref())
        .bind(payment_id)
        .execute(db)
        .await?;
    }

    Ok(())
}

/// Return a user's invoices, creating missing records for prior subscriptions
/// so historical payments remain downloadable.
pub async fn list_invoices_for_user(
    db: &PgPool,
    config: &AppConfig,
    user_id: &str,
) -> Result<Vec<InvoiceRecord>, AppError> {
    backfill_invoices_for_user(db, config, user_id).await?;
    let invoices = sqlx::query_as::<_, InvoiceRecord>(
        r#"SELECT id, number, interval, amount_minor, currency, status, paid_at,
                  period_start, period_end, gateway_order_id, gateway_payment_id
           FROM invoices
           WHERE user_id = $1
           ORDER BY paid_at DESC, id DESC"#,
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;

    Ok(invoices)
}

/// Fetch an invoice by id, scoped to the requesting user. Mirrors
/// `getInvoiceForUser` from `server/billing.ts`.
pub async fn get_invoice_for_user(
    db: &PgPool,
    config: &AppConfig,
    user_id: &str,
    invoice_id: &str,
) -> Result<Option<InvoiceRecord>, AppError> {
    backfill_invoices_for_user(db, config, user_id).await?;
    let invoice = sqlx::query_as::<_, InvoiceRecord>(
        r#"SELECT id, number, interval, amount_minor, currency, status, paid_at,
                  period_start, period_end, gateway_order_id, gateway_payment_id
           FROM invoices
           WHERE id = $1 AND user_id = $2"#,
    )
    .bind(invoice_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?;

    Ok(invoice)
}
