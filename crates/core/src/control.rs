//! UI → crawler commands through the `control_commands` table (see
//! `brainstorms/11-process-architecture.md`). The UI submits; the crawler polls, applies,
//! and marks each row done.

use sqlx::SqlitePool;

use crate::time::now_ms;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Pause,
    Resume,
    Stop,
}

impl Command {
    pub fn as_str(self) -> &'static str {
        return match self {
            Command::Pause => "pause",
            Command::Resume => "resume",
            Command::Stop => "stop",
        };
    }

    pub fn parse(s: &str) -> Option<Self> {
        return match s {
            "pause" => Some(Command::Pause),
            "resume" => Some(Command::Resume),
            "stop" => Some(Command::Stop),
            _ => None,
        };
    }
}

pub async fn submit(pool: &SqlitePool, command: Command) -> anyhow::Result<i64> {
    let id =
        sqlx::query_scalar("INSERT INTO control_commands (ts, command) VALUES (?, ?) RETURNING id")
            .bind(now_ms())
            .bind(command.as_str())
            .fetch_one(pool)
            .await?;
    return Ok(id);
}

/// Pending commands, oldest first, as `(id, command)`. Unknown commands come back as `None`.
pub async fn pending(pool: &SqlitePool) -> anyhow::Result<Vec<(i64, Option<Command>)>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, command FROM control_commands WHERE status = 'pending' ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    return Ok(rows
        .into_iter()
        .map(|(id, c)| (id, Command::parse(&c)))
        .collect());
}

pub async fn mark(pool: &SqlitePool, id: i64, status: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE control_commands SET status = ? WHERE id = ?")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await?;
    return Ok(());
}

/// Marks commands left over from before this crawler started as expired, so a stale
/// "stop" can't end a new run. Returns how many.
pub async fn expire_pending(pool: &SqlitePool) -> anyhow::Result<u64> {
    let result =
        sqlx::query("UPDATE control_commands SET status = 'expired' WHERE status = 'pending'")
            .execute(pool)
            .await?;
    return Ok(result.rows_affected());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    #[tokio::test]
    async fn submit_poll_and_expire() {
        let (_dir, pool) = test_pool().await;
        let a = submit(&pool, Command::Pause).await.unwrap();
        submit(&pool, Command::Stop).await.unwrap();
        assert_eq!(expire_pending(&pool).await.unwrap(), 2);
        assert!(pending(&pool).await.unwrap().is_empty());

        let b = submit(&pool, Command::Resume).await.unwrap();
        assert!(b > a);
        assert_eq!(pending(&pool).await.unwrap(), [(b, Some(Command::Resume))]);
        mark(&pool, b, "done").await.unwrap();
        assert!(pending(&pool).await.unwrap().is_empty());
    }
}
