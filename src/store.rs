use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::record::RequestRecord;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_ms INTEGER NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                path TEXT NOT NULL,
                status INTEGER NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_read_tokens INTEGER NOT NULL,
                cache_write_tokens INTEGER NOT NULL,
                ttft_ms INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                cost_usd REAL NOT NULL,
                streamed INTEGER NOT NULL,
                estimated INTEGER NOT NULL,
                request_body TEXT,
                response_body TEXT
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert(
        &self,
        rec: &RequestRecord,
        request_body: &str,
        response_body: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO requests (ts_ms, provider, model, path, status,
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                ttft_ms, duration_ms, cost_usd, streamed, estimated,
                request_body, response_body)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                rec.ts_ms,
                rec.provider,
                rec.model,
                rec.path,
                rec.status,
                rec.input_tokens,
                rec.output_tokens,
                rec.cache_read_tokens,
                rec.cache_write_tokens,
                rec.ttft_ms,
                rec.duration_ms,
                rec.cost_usd,
                rec.streamed,
                rec.estimated,
                request_body,
                response_body,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// A record plus its request body, or the closest earlier request of the
    /// same provider/model/path — the previous turn of the same agent loop.
    pub fn with_body(
        &self,
        id: i64,
        prev_of: Option<&RequestRecord>,
    ) -> Result<Option<(RequestRecord, String)>> {
        let conn = self.conn.lock().unwrap();
        let (sql, p): (&str, Vec<rusqlite::types::Value>) = match prev_of {
            None => (
                "SELECT id, ts_ms, provider, model, path, status,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                        ttft_ms, duration_ms, cost_usd, streamed, estimated, request_body
                 FROM requests WHERE id = ?1",
                vec![id.into()],
            ),
            Some(r) => (
                "SELECT id, ts_ms, provider, model, path, status,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                        ttft_ms, duration_ms, cost_usd, streamed, estimated, request_body
                 FROM requests
                 WHERE id < ?1 AND provider = ?2 AND model = ?3 AND path = ?4
                 ORDER BY id DESC LIMIT 1",
                vec![
                    r.id.into(),
                    r.provider.clone().into(),
                    r.model.clone().into(),
                    r.path.clone().into(),
                ],
            ),
        };
        let mut stmt = conn.prepare_cached(sql)?;
        let mut rows = stmt.query_map(rusqlite::params_from_iter(p), |r| {
            Ok((
                RequestRecord {
                    id: r.get(0)?,
                    ts_ms: r.get(1)?,
                    provider: r.get(2)?,
                    model: r.get(3)?,
                    path: r.get(4)?,
                    status: r.get(5)?,
                    input_tokens: r.get(6)?,
                    output_tokens: r.get(7)?,
                    cache_read_tokens: r.get(8)?,
                    cache_write_tokens: r.get(9)?,
                    ttft_ms: r.get(10)?,
                    duration_ms: r.get(11)?,
                    cost_usd: r.get(12)?,
                    streamed: r.get(13)?,
                    estimated: r.get(14)?,
                },
                r.get::<_, Option<String>>(15)?.unwrap_or_default(),
            ))
        })?;
        Ok(rows.next().transpose()?)
    }

    /// Records with id > `since`, ascending, bodies excluded.
    pub fn recent(&self, since: i64, limit: i64) -> Result<Vec<RequestRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, ts_ms, provider, model, path, status,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    ttft_ms, duration_ms, cost_usd, streamed, estimated
             FROM requests WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since, limit], |r| {
            Ok(RequestRecord {
                id: r.get(0)?,
                ts_ms: r.get(1)?,
                provider: r.get(2)?,
                model: r.get(3)?,
                path: r.get(4)?,
                status: r.get(5)?,
                input_tokens: r.get(6)?,
                output_tokens: r.get(7)?,
                cache_read_tokens: r.get(8)?,
                cache_write_tokens: r.get(9)?,
                ttft_ms: r.get(10)?,
                duration_ms: r.get(11)?,
                cost_usd: r.get(12)?,
                streamed: r.get(13)?,
                estimated: r.get(14)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

pub fn default_db_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("llmscope")
        .join("llmscope.db")
}
