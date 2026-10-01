//! Avatar upload business rules, mirroring
//! `@/lib/profile/avatar.ts` and the `avatarFileError` validator in
//! `@/lib/validation/index.ts` from the Next.js app.

use chrono::{DateTime, Datelike, Utc};

pub const AVATAR_CHANGES_PER_MONTH: i32 = 5;
pub const AVATAR_MAX_BYTES: i64 = 10 * 1024 * 1024;

const AVATAR_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// `YYYY-MM` key for the given (UTC) instant, e.g. `2026-03`.
pub fn avatar_month_key(date: DateTime<Utc>) -> String {
    format!("{:04}-{:02}", date.year(), date.month())
}

/// Number of avatar changes already used in `month`, resetting to 0 when
/// the stored month differs from the current one.
pub fn avatar_changes_used_this_month(count: i32, stored_month: Option<&str>, month: &str) -> i32 {
    if stored_month != Some(month) {
        return 0;
    }
    count.max(0)
}

/// Returns an error message if `used` has hit the monthly quota.
pub fn avatar_change_quota_error(used: i32) -> Option<String> {
    if used >= AVATAR_CHANGES_PER_MONTH {
        Some("You can change your photo 5 times per month. Try again next month.".to_string())
    } else {
        None
    }
}

/// Validates an uploaded avatar file's MIME type and size.
pub fn avatar_file_error(mime_type: &str, size: i64) -> Option<String> {
    if size < 1 {
        return Some("Choose an image".to_string());
    }
    if !AVATAR_MIME_TYPES.contains(&mime_type) {
        return Some("Use a PNG, JPEG, WebP, or GIF image".to_string());
    }
    if size > AVATAR_MAX_BYTES {
        return Some("Image must be 10 MB or smaller".to_string());
    }
    None
}
