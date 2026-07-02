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
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
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
        // Migration for capture files created before session grouping.
        let _ = conn.execute(
            "ALTER TABLE requests ADD COLUMN session_key TEXT NOT NULL DEFAULT ''",
            [],
        );
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
                request_body, response_body, session_key)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
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
                rec.session,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// A record plus its request and response bodies, or the closest earlier
    /// request of the same session — the previous turn of the same agent loop.
    pub fn with_body(
        &self,
        id: i64,
        prev_of: Option<&RequestRecord>,
    ) -> Result<Option<(RequestRecord, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let (sql, p): (&str, Vec<rusqlite::types::Value>) = match prev_of {
            None => (
                "SELECT id, ts_ms, provider, model, path, status,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                        ttft_ms, duration_ms, cost_usd, streamed, estimated, session_key,
                        request_body, response_body
                 FROM requests WHERE id = ?1",
                vec![id.into()],
            ),
            // Same conversation when the fingerprint is known; requests
            // captured before session grouping fall back to model matching.
            // Errored requests never completed a turn, so they can't serve
            // as the diff baseline — without the status filter the first
            // real turn after a failed run diffs against a 401 and reads
            // as a cache miss with no cause instead of "first".
            Some(r) if !r.session.is_empty() => (
                "SELECT id, ts_ms, provider, model, path, status,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                        ttft_ms, duration_ms, cost_usd, streamed, estimated, session_key,
                        request_body, response_body
                 FROM requests
                 WHERE id < ?1 AND session_key = ?2 AND status BETWEEN 200 AND 299
                 ORDER BY id DESC LIMIT 1",
                vec![r.id.into(), r.session.clone().into()],
            ),
            Some(r) => (
                "SELECT id, ts_ms, provider, model, path, status,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                        ttft_ms, duration_ms, cost_usd, streamed, estimated, session_key,
                        request_body, response_body
                 FROM requests
                 WHERE id < ?1 AND provider = ?2 AND model = ?3 AND path = ?4
                   AND status BETWEEN 200 AND 299
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
                    session: r.get(15)?,
                },
                r.get::<_, Option<String>>(16)?.unwrap_or_default(),
                r.get::<_, Option<String>>(17)?.unwrap_or_default(),
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
                    ttft_ms, duration_ms, cost_usd, streamed, estimated, session_key
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
                session: r.get(15)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id_hint: i64, status: i64, session: &str) -> RequestRecord {
        RequestRecord {
            id: 0,
            ts_ms: 1_000 + id_hint,
            provider: "openai".into(),
            model: "m".into(),
            path: "/v1/chat/completions".into(),
            status,
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            ttft_ms: 1,
            duration_ms: 2,
            cost_usd: 0.0,
            streamed: true,
            estimated: false,
            session: session.into(),
        }
    }

    #[test]
    fn errored_requests_are_skipped_as_diff_baseline() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&rec(1, 200, "s1"), "turn0", "").unwrap();
        store.insert(&rec(2, 401, "s1"), "failed", "").unwrap();
        let id3 = store.insert(&rec(3, 200, "s1"), "turn1", "").unwrap();

        let (curr, _, _) = store.with_body(id3, None).unwrap().unwrap();
        let (prev, prev_body, _) = store.with_body(0, Some(&curr)).unwrap().unwrap();
        assert_eq!(prev.status, 200);
        assert_eq!(prev_body, "turn0");

        // A session whose only earlier request errored has no baseline.
        store.insert(&rec(4, 401, "s2"), "failed", "").unwrap();
        let id5 = store.insert(&rec(5, 200, "s2"), "turn0", "").unwrap();
        let (curr, _, _) = store.with_body(id5, None).unwrap().unwrap();
        assert!(store.with_body(0, Some(&curr)).unwrap().is_none());
    }
}
