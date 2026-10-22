//! Board-role authorization helpers, mirroring the owner/board-member
//! portion of `@/server/authz.ts`.
//!
//! Access includes direct board ownership/membership and active-plan workspace
//! membership, matching the SketchFlow app's board access rules.

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
    workspace_id: Option<String>,
}

pub struct BoardAccess {
    pub board_id: String,
    pub owner_id: String,
    pub role: BoardRole,
}

/// Resolves the signed-in user's role from board ownership, direct membership,
/// or eligible workspace membership.
pub async fn get_board_access(
    db: &PgPool,
    board_id: &str,
    user_id: &str,
) -> Result<Option<BoardAccess>, AppError> {
    let Some(board) = sqlx::query_as::<_, BoardRow>(
        "SELECT id, owner_id, workspace_id FROM boards WHERE id = $1",
    )
    .bind(board_id)
    .fetch_optional(db)
    .await?
    else {
        return Ok(None);
    };

    let role = if board.owner_id == user_id {
        Some(BoardRole::Owner)
    } else {
        let direct_role = sqlx::query_scalar::<_, String>(
            "SELECT role::text FROM board_members WHERE board_id = $1 AND user_id = $2",
        )
        .bind(board_id)
        .bind(user_id)
        .fetch_optional(db)
        .await?
        .and_then(|role| BoardRole::from_db(&role));
        match direct_role {
            Some(role) => Some(role),
            None => workspace_fallback_role(db, board.workspace_id.as_deref(), user_id).await?,
        }
    };

    Ok(role.map(|role| BoardAccess {
        board_id: board.id,
        owner_id: board.owner_id,
        role,
    }))
}

async fn workspace_fallback_role(
    db: &PgPool,
    workspace_id: Option<&str>,
    user_id: &str,
) -> Result<Option<BoardRole>, AppError> {
    let Some(workspace_id) = workspace_id else {
        return Ok(None);
    };
    let workspace_role = sqlx::query_scalar::<_, String>(
        "SELECT role::text FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    let Some(workspace_role) = workspace_role else {
        return Ok(None);
    };
    if crate::billing::get_billing_state(db, user_id).await?.status
        != crate::billing::BillingStatus::Active
    {
        return Ok(None);
    }
    Ok(match workspace_role.as_str() {
        "OWNER" | "ADMIN" => Some(BoardRole::Editor),
        "MEMBER" => Some(BoardRole::Viewer),
        _ => None,
    })
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
