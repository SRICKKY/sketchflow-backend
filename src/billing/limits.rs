//! Plan limit constants and checks, mirroring `@/lib/billing/limits.ts`
//! and the subset of `@/server/planLimits.ts` needed by dashboard and upload APIs.

pub const FREE_MAX_OWNED_BOARDS: i64 = 5;
pub const FREE_MAX_BOARD_MEMBERS: i64 = 2;

pub const FREE_MAX_IMAGES_PER_BOARD: i64 = 10;
pub const PRO_MAX_IMAGES_PER_BOARD: i64 = 100;
pub const FREE_MAX_IMAGE_BYTES: i64 = 5 * 1024 * 1024;
pub const PRO_MAX_IMAGE_BYTES: i64 = 10 * 1024 * 1024;

pub const IMAGE_COUNT_MESSAGE: &str =
    "Free plan allows 10 images per board. Upgrade to Pro for up to 100.";
pub fn image_size_message(is_pro: bool) -> &'static str {
    if is_pro {
        "This image exceeds the 10 MB limit for the Pro plan."
    } else {
        "This image exceeds the 5 MB limit for the Free plan. Upgrade to Pro for uploads up to 10 MB."
    }
}

pub fn image_count_cap(is_pro: bool) -> i64 {
    if is_pro {
        PRO_MAX_IMAGES_PER_BOARD
    } else {
        FREE_MAX_IMAGES_PER_BOARD
    }
}

pub fn image_byte_cap(is_pro: bool) -> i64 {
    if is_pro {
        PRO_MAX_IMAGE_BYTES
    } else {
        FREE_MAX_IMAGE_BYTES
    }
}

#[cfg(test)]
mod tests {
    use super::image_size_message;

    #[test]
    fn image_size_message_matches_the_plan_limit() {
        assert_eq!(
            image_size_message(true),
            "This image exceeds the 10 MB limit for the Pro plan."
        );
        assert_eq!(
            image_size_message(false),
            "This image exceeds the 5 MB limit for the Free plan. Upgrade to Pro for uploads up to 10 MB."
        );
    }
}
