//! Thumbnail cache — SQLite + in-memory LRU fallback

use std::path::{Path, PathBuf};
use std::sync::Arc;
use parking_lot::RwLock;
use rusqlite::{params, Connection, OpenFlags};
use anyhow::Result;
use tracing::info;

#[derive(Debug, Clone)]
pub struct ThumbCacheConfig {
    pub max_entries: usize,
    pub db_path: PathBuf,
    pub thumb_size: (u32, u32),
    pub jpeg_quality: u8,
}

impl Default for ThumbCacheConfig {
    fn default() -> Self {
        let db_path = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ApertureNeoTurbo")
            .join("thumbs")
            .join("cache.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).ok();
        Self {
            max_entries: 2000,
            db_path,
            thumb_size: (200, 200),
            jpeg_quality: 85,
        }
    }
}

pub struct ThumbCache {
    conn: Arc<RwLock<Connection>>,
    mem_cache: Arc<RwLock<lru::LruCache<String, Vec<u8>>>>,
    config: ThumbCacheConfig,
}

impl ThumbCache {
    pub fn new(config: ThumbCacheConfig) -> Result<Self> {
        std::fs::create_dir_all(config.db_path.parent().unwrap())?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(&config.db_path, flags)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -4096;
             PRAGMA temp_store = MEMORY;",
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS thumbnails (
                path TEXT PRIMARY KEY,
                mtime INTEGER NOT NULL,
                data BLOB NOT NULL,
                width INTEGER NOT NULL,
                height INTEGER NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_created_at ON thumbnails(created_at);",
            [],
        )?;
        let mem_cache = Arc::new(RwLock::new(lru::LruCache::new(
            std::num::NonZeroUsize::new(config.max_entries.min(500)).unwrap(),
        )));
        Ok(Self { conn: Arc::new(RwLock::new(conn)), mem_cache, config })
    }

    pub fn get(&self, path: &Path, mtime: u64) -> Option<Vec<u8>> {
        let key = path.to_string_lossy().to_string();
        let from_mem = {
            let cache = self.mem_cache.read();
            cache.peek(&key).cloned()
        };
        if let Some(data) = from_mem {
            return Some(data);
        }
        let conn = self.conn.read();
        let mut stmt = conn.prepare("SELECT data FROM thumbnails WHERE path = ?1 AND mtime = ?2").ok()?;
        let data: Option<Vec<u8>> = stmt.query_row(params![&key, mtime as i64], |row| row.get(0)).ok();
        if let Some(ref d) = data {
            self.mem_cache.write().put(key, d.clone());
        }
        data
    }

    pub fn put(&self, path: &Path, mtime: u64, data: &[u8], w: u32, h: u32) -> Result<()> {
        let key = path.to_string_lossy().to_string();
        {
            let mut cache = self.mem_cache.write();
            cache.put(key.clone(), data.to_vec());
        }
        let conn = self.conn.read();
        conn.execute(
            "INSERT OR REPLACE INTO thumbnails (path, mtime, data, width, height, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'))",
            params![&key, mtime as i64, data, w as i32, h as i32],
        )?;
        self.maybe_evict()
    }

    fn maybe_evict(&self) -> Result<()> {
        let conn = self.conn.read();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM thumbnails", [], |r| r.get(0))?;
        if count > self.config.max_entries as i64 {
            let to_delete = count - self.config.max_entries as i64 + 100;
            conn.execute(
                "DELETE FROM thumbnails WHERE path IN (
                    SELECT path FROM thumbnails ORDER BY created_at ASC LIMIT ?1
                )",
                params![to_delete],
            )?;
            info!("Evicted {} old thumbnails", to_delete);
        }
        Ok(())
    }
}