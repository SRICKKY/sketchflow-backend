//! Plan limit constants and checks, mirroring `@/lib/billing/limits.ts`
//! and the subset of `@/server/planLimits.ts` needed by the endpoints this
//! backend exposes (board image uploads).

pub const FREE_MAX_IMAGES_PER_BOARD: i64 = 10;
pub const PRO_MAX_IMAGES_PER_BOARD: i64 = 100;
pub const FREE_MAX_IMAGE_BYTES: i64 = 5 * 1024 * 1024;
pub const PRO_MAX_IMAGE_BYTES: i64 = 10 * 1024 * 1024;

pub const IMAGE_COUNT_MESSAGE: &str =
    "Free plan allows 10 images per board. Upgrade to Pro for up to 100.";
pub const IMAGE_SIZE_MESSAGE: &str =
    "Free plan allows images up to 5 MB. Upgrade to Pro for 10 MB files.";

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
