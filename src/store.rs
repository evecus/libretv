//! 服务端持久化：SQLite 单文件 KV 存储。
//!
//! 只保存「实际有用的数据」——源配置/订阅/设置快照、播放历史、播放进度、搜索记录。
//! 搜索、详情、EPG、画质、测活等 TTL 缓存一律留在内存（TtlCache），不写入本库。
//!
//! 表结构刻意做成通用 KV：`user_data(section, key, value, updated_at)`。
//! 前端每张远程表对应一个 section，每行是一条记录（value 为 JSON），
//! 设置快照则只占 settings 分区的一行。这样新增数据类别无需改表结构。

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use crate::{error::ApiError, util::now_ms};

/// 允许写入的数据分区白名单（前端 server-data.ts 与之保持一致）
pub const SECTIONS: &[&str] = &["history", "progress", "searchHistory", "settings"];

/// 单个分区最多保留的记录数，防止误写撑爆数据库
const MAX_ITEMS_PER_SECTION: usize = 5000;
/// 单条记录 JSON 上限（256KB）
const MAX_VALUE_BYTES: usize = 256 * 1024;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Arc<Self>, ApiError> {
        std::fs::create_dir_all(dir).map_err(|e| {
            ApiError::Internal(format!("创建数据目录 {} 失败: {e}", dir.display()))
        })?;
        let path = dir.join("libretv.db");
        let conn = Connection::open(&path).map_err(|e| {
            ApiError::Internal(format!("打开数据库 {} 失败: {e}", path.display()))
        })?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS user_data (
                 section     TEXT NOT NULL,
                 key         TEXT NOT NULL,
                 value       TEXT NOT NULL,
                 updated_at  INTEGER NOT NULL,
                 PRIMARY KEY (section, key)
             );
             CREATE INDEX IF NOT EXISTS idx_user_data_section ON user_data(section);",
        )
        .map_err(|e| ApiError::Internal(format!("初始化数据库失败: {e}")))?;
        Ok(Arc::new(Self { conn: Mutex::new(conn) }))
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, ApiError> {
        self.conn
            .lock()
            .map_err(|e| ApiError::Internal(format!("数据库锁异常: {e}")))
    }

    pub fn db_path(dir: &Path) -> std::path::PathBuf {
        dir.join("libretv.db")
    }

    /// 全量拉取：供前端启动时一次性 hydrate
    /// 返回 `{ "sections": { history: {key: value, ...}, ... } }`
    pub fn all(&self) -> Result<Value, ApiError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT section, key, value FROM user_data")
            .map_err(|e| ApiError::Internal(format!("查询失败: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| ApiError::Internal(format!("查询失败: {e}")))?;

        let mut sections = Map::new();
        for s in SECTIONS {
            sections.insert((*s).to_string(), Value::Object(Map::new()));
        }
        for row in rows {
            let (section, key, raw) = row.map_err(|e| ApiError::Internal(format!("读取行失败: {e}")))?;
            if !SECTIONS.contains(&section.as_str()) {
                continue;
            }
            let value: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            if let Some(Value::Object(m)) = sections.get_mut(&section) {
                m.insert(key, value);
            }
        }
        Ok(json!({ "sections": Value::Object(sections) }))
    }

    /// 批量 upsert（前端 put / bulkPut / update 都走这里）
    pub fn upsert(&self, section: &str, items: &Map<String, Value>) -> Result<usize, ApiError> {
        if items.is_empty() {
            return Ok(0);
        }
        let conn = self.lock()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| ApiError::Internal(format!("开启事务失败: {e}")))?;
        let now = now_ms();
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO user_data(section, key, value, updated_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(section, key) DO UPDATE SET
                       value = excluded.value, updated_at = excluded.updated_at",
                )
                .map_err(|e| ApiError::Internal(format!("准备写入失败: {e}")))?;
            for (key, value) in items {
                let raw = serde_json::to_string(value)
                    .map_err(|e| ApiError::BadRequest(format!("记录无法序列化: {e}")))?;
                if raw.len() > MAX_VALUE_BYTES {
                    return Err(ApiError::BadRequest(format!(
                        "单条记录过大（{} 字节，上限 {}）",
                        raw.len(),
                        MAX_VALUE_BYTES
                    )));
                }
                stmt.execute(params![section, key, raw, now])
                    .map_err(|e| ApiError::Internal(format!("写入失败: {e}")))?;
            }
        }
        tx.commit()
            .map_err(|e| ApiError::Internal(format!("提交事务失败: {e}")))?;
        self.trim(conn, section)?;
        Ok(items.len())
    }

    /// keys 为 None 时清空整个分区
    pub fn delete(&self, section: &str, keys: Option<&[String]>) -> Result<usize, ApiError> {
        let conn = self.lock()?;
        match keys {
            None => conn
                .execute("DELETE FROM user_data WHERE section = ?1", params![section])
                .map_err(|e| ApiError::Internal(format!("清空分区失败: {e}"))),
            Some(list) => {
                let tx = conn
                    .unchecked_transaction()
                    .map_err(|e| ApiError::Internal(format!("开启事务失败: {e}")))?;
                let mut n = 0;
                {
                    let mut stmt = tx
                        .prepare("DELETE FROM user_data WHERE section = ?1 AND key = ?2")
                        .map_err(|e| ApiError::Internal(format!("准备删除失败: {e}")))?;
                    for key in list {
                        n += stmt
                            .execute(params![section, key])
                            .map_err(|e| ApiError::Internal(format!("删除失败: {e}")))?;
                    }
                }
                tx.commit()
                    .map_err(|e| ApiError::Internal(format!("提交事务失败: {e}")))?;
                Ok(n)
            }
        }
    }

    /// 超过上限时按 updated_at 淘汰最旧的记录
    fn trim(&self, conn: MutexGuard<'_, Connection>, section: &str) -> Result<(), ApiError> {
        conn.execute(
            "DELETE FROM user_data WHERE section = ?1 AND key NOT IN (
                 SELECT key FROM user_data WHERE section = ?1
                 ORDER BY updated_at DESC LIMIT ?2
             )",
            params![section, MAX_ITEMS_PER_SECTION as i64],
        )
        .map_err(|e| ApiError::Internal(format!("淘汰旧记录失败: {e}")))?;
        Ok(())
    }

    /// 供诊断使用：各分区的记录数
    #[allow(dead_code)]
    pub fn stats(&self) -> Result<HashMap<String, usize>, ApiError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT section, COUNT(*) FROM user_data GROUP BY section")
            .map_err(|e| ApiError::Internal(format!("统计失败: {e}")))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?)))
            .map_err(|e| ApiError::Internal(format!("统计失败: {e}")))?;
        let mut out = HashMap::new();
        for s in SECTIONS {
            out.insert((*s).to_string(), 0);
        }
        for row in rows.flatten() {
            out.insert(row.0, row.1);
        }
        Ok(out)
    }
}
