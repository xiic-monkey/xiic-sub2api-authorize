//! 本地凭证库（SQLite）。仅存放 CDK，供重授权流程自动读取。
//!
//! 安全说明：本地单文件库，明文存储。生产环境建议再加一层主密码加密，
//! 本文件保留明文以便简单可用。

use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;

/// 凭证库文件名（位于当前工作目录，已 gitignore）。
pub const DB_FILE: &str = "credentials.db";

#[derive(Debug, Clone)]
pub struct Credential {
    pub cdk: String,
    pub updated_at: String,
}

pub fn db_path() -> PathBuf {
    PathBuf::from(DB_FILE)
}

fn open() -> Result<Connection> {
    let conn = Connection::open(db_path())?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS credentials (
            id         INTEGER PRIMARY KEY CHECK (id = 1),
            cdk        TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL DEFAULT ''
        )",
    )?;
    Ok(conn)
}

/// 写入（或覆盖）唯一一条 CDK 记录。
pub fn save(cdk: &str) -> Result<()> {
    let conn = open()?;
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "INSERT INTO credentials (id, cdk, updated_at)
         VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET
            cdk        = excluded.cdk,
            updated_at = excluded.updated_at",
        rusqlite::params![cdk, now],
    )?;
    Ok(())
}

/// 读取当前 CDK（无则返回 None）。
pub fn load() -> Result<Option<Credential>> {
    let conn = open()?;
    let mut stmt = conn.prepare("SELECT cdk, updated_at FROM credentials WHERE id = 1")?;
    let mut rows = stmt.query(rusqlite::params![])?;
    if let Some(row) = rows.next()? {
        Ok(Some(Credential {
            cdk: row.get(0)?,
            updated_at: row.get(1)?,
        }))
    } else {
        Ok(None)
    }
}
