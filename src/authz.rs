//! Board-role authorization helpers, mirroring the owner/board-member
//! portion of `@/server/authz.ts`.
//!
//! The Next.js app also grants access via workspace membership
//! (`Workspace`/`WorkspaceMember`), but those tables aren't part of the
//! endpoints this backend exposes (see `docs/API_ENDPOINTS.md`), so only
//! direct board ownership and `board_members` rows are considered here.

use sqlx::{FromRow, PgPool};

use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BoardRole {
    Viewer,
    Editor,
    Owner,
}

impl BoardRole {
    fn from_db(value: &str) -> Option<Self> {
        match value {
            "OWNER" => Some(Self::Owner),
            "EDITOR" => Some(Self::Editor),
            "VIEWER" => Some(Self::Viewer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
struct BoardRow {
    id: String,
    owner_id: String,
}

pub struct BoardAccess {
    pub board_id: String,
    pub owner_id: String,
    pub role: BoardRole,
}

/// Resolves the signed-in user's role on `board_id`: `OWNER` if they own
/// it, otherwise whatever `board_members` row (if any) exists for them.
pub async fn get_board_access(
    db: &PgPool,
    board_id: &str,
    user_id: &str,
) -> Result<Option<BoardAccess>, AppError> {
    let Some(board) =
        sqlx::query_as::<_, BoardRow>("SELECT id, owner_id FROM boards WHERE id = $1")
            .bind(board_id)
            .fetch_optional(db)
            .await?
    else {
        return Ok(None);
    };

    let role = if board.owner_id == user_id {
        Some(BoardRole::Owner)
    } else {
        sqlx::query_scalar::<_, String>(
            "SELECT role::text FROM board_members WHERE board_id = $1 AND user_id = $2",
        )
        .bind(board_id)
        .bind(user_id)
        .fetch_optional(db)
        .await?
        .and_then(|role| BoardRole::from_db(&role))
    };

    Ok(role.map(|role| BoardAccess {
        board_id: board.id,
        owner_id: board.owner_id,
        role,
    }))
}

/// Requires the signed-in user to have at least `min_role` on `board_id`.
pub async fn require_board_role(
    db: &PgPool,
    board_id: &str,
    user_id: &str,
    min_role: BoardRole,
) -> Result<BoardAccess, AppError> {
    let access = get_board_access(db, board_id, user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Board not found".to_string()))?;
    if access.role < min_role {
        return Err(AppError::Forbidden(
            "You do not have permission to do that.".to_string(),
        ));
    }
    Ok(access)
}
