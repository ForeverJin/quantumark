// SQLite 存储层。
//
// 设计说明：
//   - 单连接 + std Mutex，所有调用都在 spawn_blocking 里跑（见调用方），
//     写法简单、不给 SQLite 并发添乱。
//   - 迁移用「CREATE TABLE IF NOT EXISTS + PRAGMA user_version」，够用了。
//   - 索引表 files 是「115 上有什么」的唯一事实来源：为了不触发 115 列目录配额，
//     除首次认领外不扫描远端目录。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{
    ArchiveRow, DestinationRow, FileRow, JobRow, PendingRow, Policy, RunRow, RunStats,
};

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("创建数据目录 {} 失败: {e}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("打开数据库失败: {e}"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )
        .map_err(|e| format!("设置数据库参数失败: {e}"))?;

        let store = Store { conn: Mutex::new(conn) };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), String> {
        let conn = self.lock();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS destinations (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                name       TEXT NOT NULL UNIQUE,
                kind       TEXT NOT NULL,
                config     TEXT NOT NULL,
                secret     TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS jobs (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                name           TEXT NOT NULL UNIQUE,
                enabled        INTEGER NOT NULL DEFAULT 1,
                kind           TEXT NOT NULL,
                destination_id INTEGER NOT NULL,
                remote_dir     TEXT NOT NULL,
                cron           TEXT,
                keep           INTEGER,
                options        TEXT NOT NULL,
                secret         TEXT,
                created_at     INTEGER NOT NULL,
                updated_at     INTEGER NOT NULL,
                FOREIGN KEY (destination_id) REFERENCES destinations(id)
            );

            CREATE TABLE IF NOT EXISTS runs (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id      INTEGER NOT NULL,
                trigger     TEXT NOT NULL,
                status      TEXT NOT NULL,
                started_at  INTEGER NOT NULL,
                finished_at INTEGER,
                stats       TEXT,
                error       TEXT,
                log_path    TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_runs_job ON runs(job_id, id DESC);

            CREATE TABLE IF NOT EXISTS files (
                job_id      INTEGER NOT NULL,
                rel         TEXT NOT NULL,
                size        INTEGER NOT NULL,
                mtime_ns    INTEGER NOT NULL,
                sha1        TEXT,
                remote_path TEXT NOT NULL,
                updated_at  INTEGER NOT NULL,
                PRIMARY KEY (job_id, rel)
            );

            CREATE TABLE IF NOT EXISTS pending_uploads (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id           INTEGER NOT NULL,
                remote_temp_path TEXT NOT NULL,
                created_at       INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS archives (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id      INTEGER NOT NULL,
                remote_path TEXT NOT NULL UNIQUE,
                size        INTEGER NOT NULL,
                sha1        TEXT,
                created_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_archives_job ON archives(job_id, id DESC);

            -- 网页登录会话：存库，容器重启后不用重新登录
            CREATE TABLE IF NOT EXISTS sessions (
                token     TEXT PRIMARY KEY,
                csrf      TEXT NOT NULL,
                last_seen INTEGER NOT NULL
            );

            -- 已确认存在的远端目录，避免重启后为每个目录再查一次（列的每一次查询都算配额）
            CREATE TABLE IF NOT EXISTS remote_dirs (
                job_id INTEGER NOT NULL,
                path   TEXT NOT NULL,
                PRIMARY KEY (job_id, path)
            );
            "#,
        )
        .map_err(|e| format!("建表失败: {e}"))?;
        conn.pragma_update(None, "user_version", 1)
            .map_err(|e| format!("写入 schema 版本失败: {e}"))?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // 连接被 panic 毒化时继续用：SQLite 本身没有跨语句的中间状态
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---------- 设置 ----------

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, String> {
        let conn = self.lock();
        conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .map_err(|e| format!("读设置失败: {e}"))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO settings(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(|e| format!("写设置失败: {e}"))?;
        Ok(())
    }

    pub fn policy(&self) -> Policy {
        let raw = self.get_setting("policy").ok().flatten();
        Policy::from_json_or_default(raw.as_deref())
    }

    pub fn set_policy(&self, policy: &Policy) -> Result<(), String> {
        let json = serde_json::to_string(policy).map_err(|e| format!("策略序列化失败: {e}"))?;
        self.set_setting("policy", &json)
    }

    // ---------- 目的地 ----------

    pub fn list_destinations(&self) -> Result<Vec<DestinationRow>, String> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, kind, config, secret, created_at, updated_at
                 FROM destinations ORDER BY id",
            )
            .map_err(|e| format!("查询目的地失败: {e}"))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(DestinationRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    config_json: r.get(3)?,
                    secret_sealed: r.get(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            })
            .map_err(|e| format!("查询目的地失败: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取目的地失败: {e}"))
    }

    pub fn get_destination(&self, id: i64) -> Result<Option<DestinationRow>, String> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, name, kind, config, secret, created_at, updated_at
             FROM destinations WHERE id = ?1",
            params![id],
            |r| {
                Ok(DestinationRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    config_json: r.get(3)?,
                    secret_sealed: r.get(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            },
        )
        .optional()
        .map_err(|e| format!("读取目的地失败: {e}"))
    }

    pub fn insert_destination(
        &self,
        name: &str,
        kind: &str,
        config_json: &str,
        secret_sealed: Option<&str>,
    ) -> Result<i64, String> {
        let now = now();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO destinations(name, kind, config, secret, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?5)",
            params![name, kind, config_json, secret_sealed, now],
        )
        .map_err(|e| format!("新增目的地失败: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_destination(
        &self,
        id: i64,
        name: &str,
        config_json: &str,
        secret_sealed: Option<&str>,
    ) -> Result<(), String> {
        let now = now();
        let conn = self.lock();
        match secret_sealed {
            Some(s) => conn.execute(
                "UPDATE destinations SET name=?1, config=?2, secret=?3, updated_at=?4 WHERE id=?5",
                params![name, config_json, s, now, id],
            ),
            None => conn.execute(
                "UPDATE destinations SET name=?1, config=?2, updated_at=?3 WHERE id=?4",
                params![name, config_json, now, id],
            ),
        }
        .map_err(|e| format!("更新目的地失败: {e}"))?;
        Ok(())
    }

    pub fn delete_destination(&self, id: i64) -> Result<(), String> {
        let conn = self.lock();
        // 有任务在用就不许删
        let used: i64 = conn
            .query_row("SELECT COUNT(*) FROM jobs WHERE destination_id = ?1", params![id], |r| r.get(0))
            .map_err(|e| format!("检查目的地引用失败: {e}"))?;
        if used > 0 {
            return Err(format!("还有 {used} 个任务在用这个目的地，先删任务"));
        }
        conn.execute("DELETE FROM destinations WHERE id = ?1", params![id])
            .map_err(|e| format!("删除目的地失败: {e}"))?;
        Ok(())
    }

    // ---------- 任务 ----------

    fn job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
        Ok(JobRow {
            id: r.get(0)?,
            name: r.get(1)?,
            enabled: r.get::<_, i64>(2)? != 0,
            kind: r.get(3)?,
            destination_id: r.get(4)?,
            remote_dir: r.get(5)?,
            cron: r.get(6)?,
            keep: r.get(7)?,
            options_json: r.get(8)?,
            secret_sealed: r.get(9)?,
            created_at: r.get(10)?,
            updated_at: r.get(11)?,
        })
    }

    pub fn list_jobs(&self) -> Result<Vec<JobRow>, String> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, enabled, kind, destination_id, remote_dir, cron, keep,
                        options, secret, created_at, updated_at
                 FROM jobs ORDER BY id",
            )
            .map_err(|e| format!("查询任务失败: {e}"))?;
        let rows = stmt
            .query_map([], Store::job_from_row)
            .map_err(|e| format!("查询任务失败: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取任务失败: {e}"))
    }

    pub fn get_job(&self, id: i64) -> Result<Option<JobRow>, String> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, name, enabled, kind, destination_id, remote_dir, cron, keep,
                    options, secret, created_at, updated_at
             FROM jobs WHERE id = ?1",
            params![id],
            Store::job_from_row,
        )
        .optional()
        .map_err(|e| format!("读取任务失败: {e}"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_job(
        &self,
        name: &str,
        enabled: bool,
        kind: &str,
        destination_id: i64,
        remote_dir: &str,
        cron: Option<&str>,
        keep: Option<i64>,
        options_json: &str,
        secret_sealed: Option<&str>,
    ) -> Result<i64, String> {
        let now = now();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO jobs(name, enabled, kind, destination_id, remote_dir, cron, keep,
                              options, secret, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                name,
                enabled as i64,
                kind,
                destination_id,
                remote_dir,
                cron,
                keep,
                options_json,
                secret_sealed,
                now
            ],
        )
        .map_err(|e| format!("新增任务失败: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_job(
        &self,
        id: i64,
        name: &str,
        enabled: bool,
        destination_id: i64,
        remote_dir: &str,
        cron: Option<&str>,
        keep: Option<i64>,
        options_json: &str,
        secret_sealed: Option<&str>,
    ) -> Result<(), String> {
        let now = now();
        let conn = self.lock();
        match secret_sealed {
            Some(s) => conn.execute(
                "UPDATE jobs SET name=?1, enabled=?2, destination_id=?3, remote_dir=?4, cron=?5,
                                 keep=?6, options=?7, secret=?8, updated_at=?9
                 WHERE id=?10",
                params![name, enabled as i64, destination_id, remote_dir, cron, keep, options_json, s, now, id],
            ),
            None => conn.execute(
                "UPDATE jobs SET name=?1, enabled=?2, destination_id=?3, remote_dir=?4, cron=?5,
                                 keep=?6, options=?7, updated_at=?8
                 WHERE id=?9",
                params![name, enabled as i64, destination_id, remote_dir, cron, keep, options_json, now, id],
            ),
        }
        .map_err(|e| format!("更新任务失败: {e}"))?;
        Ok(())
    }

    pub fn delete_job(&self, id: i64) -> Result<(), String> {
        let conn = self.lock();
        conn.execute("DELETE FROM files WHERE job_id = ?1", params![id])
            .map_err(|e| format!("删除任务失败: {e}"))?;
        conn.execute("DELETE FROM pending_uploads WHERE job_id = ?1", params![id])
            .map_err(|e| format!("删除任务失败: {e}"))?;
        conn.execute("DELETE FROM archives WHERE job_id = ?1", params![id])
            .map_err(|e| format!("删除任务失败: {e}"))?;
        conn.execute("DELETE FROM remote_dirs WHERE job_id = ?1", params![id])
            .map_err(|e| format!("删除任务失败: {e}"))?;
        conn.execute("DELETE FROM runs WHERE job_id = ?1", params![id])
            .map_err(|e| format!("删除任务失败: {e}"))?;
        conn.execute("DELETE FROM jobs WHERE id = ?1", params![id])
            .map_err(|e| format!("删除任务失败: {e}"))?;
        Ok(())
    }

    // ---------- 运行记录 ----------

    pub fn start_run(&self, job_id: i64, trigger: &str, log_path: &str) -> Result<i64, String> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO runs(job_id, trigger, status, started_at, log_path)
             VALUES(?1, ?2, 'running', ?3, ?4)",
            params![job_id, trigger, now(), log_path],
        )
        .map_err(|e| format!("创建运行记录失败: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn finish_run(
        &self,
        id: i64,
        status: &str,
        stats: &RunStats,
        error: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET status=?1, finished_at=?2, stats=?3, error=?4 WHERE id=?5",
            params![status, now(), stats.to_json(), error, id],
        )
        .map_err(|e| format!("更新运行记录失败: {e}"))?;
        Ok(())
    }

    /// 程序重启时把遗留的 running 记录标成失败。
    pub fn fail_stale_runs(&self) -> Result<usize, String> {
        let conn = self.lock();
        let n = conn
            .execute(
                "UPDATE runs SET status='failed', finished_at=?1,
                        error='程序重启时任务被中断'
                 WHERE status='running'",
                params![now()],
            )
            .map_err(|e| format!("清理中断记录失败: {e}"))?;
        Ok(n)
    }

    pub fn run_by_id(&self, id: i64) -> Result<Option<RunRow>, String> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, job_id, trigger, status, started_at, finished_at, stats, error, log_path
             FROM runs WHERE id = ?1",
            params![id],
            |r| {
                Ok(RunRow {
                    id: r.get(0)?,
                    job_id: r.get(1)?,
                    trigger: r.get(2)?,
                    status: r.get(3)?,
                    started_at: r.get(4)?,
                    finished_at: r.get(5)?,
                    stats_json: r.get(6)?,
                    error: r.get(7)?,
                    log_path: r.get(8)?,
                })
            },
        )
        .optional()
        .map_err(|e| format!("读取运行记录失败: {e}"))
    }

    pub fn list_runs(&self, job_id: Option<i64>, limit: i64) -> Result<Vec<RunRow>, String> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, job_id, trigger, status, started_at, finished_at, stats, error, log_path
                 FROM runs
                 WHERE (?1 IS NULL OR job_id = ?1)
                 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(|e| format!("查询运行记录失败: {e}"))?;
        let rows = stmt
            .query_map(params![job_id, limit], |r| {
                Ok(RunRow {
                    id: r.get(0)?,
                    job_id: r.get(1)?,
                    trigger: r.get(2)?,
                    status: r.get(3)?,
                    started_at: r.get(4)?,
                    finished_at: r.get(5)?,
                    stats_json: r.get(6)?,
                    error: r.get(7)?,
                    log_path: r.get(8)?,
                })
            })
            .map_err(|e| format!("查询运行记录失败: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取运行记录失败: {e}"))
    }

    // ---------- 文件索引 ----------

    pub fn load_files(&self, job_id: i64) -> Result<Vec<FileRow>, String> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT job_id, rel, size, mtime_ns, sha1, remote_path, updated_at
                 FROM files WHERE job_id = ?1",
            )
            .map_err(|e| format!("查询索引失败: {e}"))?;
        let rows = stmt
            .query_map(params![job_id], |r| {
                Ok(FileRow {
                    job_id: r.get(0)?,
                    rel: r.get(1)?,
                    size: r.get(2)?,
                    mtime_ns: r.get(3)?,
                    sha1: r.get(4)?,
                    remote_path: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            })
            .map_err(|e| format!("查询索引失败: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取索引失败: {e}"))
    }

    pub fn upsert_file(&self, row: &FileRow) -> Result<(), String> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO files(job_id, rel, size, mtime_ns, sha1, remote_path, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(job_id, rel) DO UPDATE SET
                 size = excluded.size, mtime_ns = excluded.mtime_ns, sha1 = excluded.sha1,
                 remote_path = excluded.remote_path, updated_at = excluded.updated_at",
            params![
                row.job_id,
                row.rel,
                row.size,
                row.mtime_ns,
                row.sha1,
                row.remote_path,
                row.updated_at
            ],
        )
        .map_err(|e| format!("写索引失败: {e}"))?;
        Ok(())
    }

    pub fn delete_file_row(&self, job_id: i64, rel: &str) -> Result<(), String> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM files WHERE job_id = ?1 AND rel = ?2",
            params![job_id, rel],
        )
        .map_err(|e| format!("删索引失败: {e}"))?;
        Ok(())
    }

    pub fn clear_files(&self, job_id: i64) -> Result<(), String> {
        let conn = self.lock();
        conn.execute("DELETE FROM files WHERE job_id = ?1", params![job_id])
            .map_err(|e| format!("清空索引失败: {e}"))?;
        Ok(())
    }

    // ---------- 待清理的临时上传 ----------

    pub fn add_pending(&self, job_id: i64, remote_temp_path: &str) -> Result<i64, String> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO pending_uploads(job_id, remote_temp_path, created_at) VALUES(?1, ?2, ?3)",
            params![job_id, remote_temp_path, now()],
        )
        .map_err(|e| format!("记录待清理临时文件失败: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_pending(&self, job_id: i64) -> Result<Vec<PendingRow>, String> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, job_id, remote_temp_path, created_at
                 FROM pending_uploads WHERE job_id = ?1 ORDER BY id",
            )
            .map_err(|e| format!("查询临时文件失败: {e}"))?;
        let rows = stmt
            .query_map(params![job_id], |r| {
                Ok(PendingRow {
                    id: r.get(0)?,
                    job_id: r.get(1)?,
                    remote_temp_path: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })
            .map_err(|e| format!("查询临时文件失败: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取临时文件失败: {e}"))
    }

    pub fn remove_pending(&self, id: i64) -> Result<(), String> {
        let conn = self.lock();
        conn.execute("DELETE FROM pending_uploads WHERE id = ?1", params![id])
            .map_err(|e| format!("清理临时文件记录失败: {e}"))?;
        Ok(())
    }

    // ---------- 归档 ----------

    pub fn add_archive(
        &self,
        job_id: i64,
        remote_path: &str,
        size: i64,
        sha1: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO archives(job_id, remote_path, size, sha1, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(remote_path) DO UPDATE SET size = excluded.size, sha1 = excluded.sha1",
            params![job_id, remote_path, size, sha1, now()],
        )
        .map_err(|e| format!("记录归档失败: {e}"))?;
        Ok(())
    }

    pub fn list_archives(&self, job_id: i64) -> Result<Vec<ArchiveRow>, String> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, job_id, remote_path, size, sha1, created_at
                 FROM archives WHERE job_id = ?1 ORDER BY id DESC",
            )
            .map_err(|e| format!("查询归档失败: {e}"))?;
        let rows = stmt
            .query_map(params![job_id], |r| {
                Ok(ArchiveRow {
                    id: r.get(0)?,
                    job_id: r.get(1)?,
                    remote_path: r.get(2)?,
                    size: r.get(3)?,
                    sha1: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })
            .map_err(|e| format!("查询归档失败: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取归档失败: {e}"))
    }

    pub fn remove_archive(&self, remote_path: &str) -> Result<(), String> {
        let conn = self.lock();
        conn.execute("DELETE FROM archives WHERE remote_path = ?1", params![remote_path])
            .map_err(|e| format!("删除归档记录失败: {e}"))?;
        Ok(())
    }

    // ---------- 网页会话 ----------

    pub fn put_session(&self, token: &str, csrf: &str) -> Result<(), String> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sessions(token, csrf, last_seen) VALUES(?1, ?2, ?3)
             ON CONFLICT(token) DO UPDATE SET csrf = excluded.csrf, last_seen = excluded.last_seen",
            params![token, csrf, now()],
        )
        .map_err(|e| format!("保存会话失败: {e}"))?;
        Ok(())
    }

    pub fn get_session(&self, token: &str) -> Result<Option<(String, i64)>, String> {
        let conn = self.lock();
        conn.query_row(
            "SELECT csrf, last_seen FROM sessions WHERE token = ?1",
            params![token],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|e| format!("读取会话失败: {e}"))
    }

    pub fn touch_session(&self, token: &str, ts: i64) -> Result<(), String> {
        let conn = self.lock();
        conn.execute(
            "UPDATE sessions SET last_seen = ?1 WHERE token = ?2",
            params![ts, token],
        )
        .map_err(|e| format!("更新会话失败: {e}"))?;
        Ok(())
    }

    pub fn drop_session(&self, token: &str) -> Result<(), String> {
        let conn = self.lock();
        conn.execute("DELETE FROM sessions WHERE token = ?1", params![token])
            .map_err(|e| format!("删除会话失败: {e}"))?;
        Ok(())
    }

    /// 清掉超过保留期的会话（启动时调用）。
    pub fn prune_sessions(&self, cutoff: i64) -> Result<usize, String> {
        let conn = self.lock();
        conn.execute("DELETE FROM sessions WHERE last_seen < ?1", params![cutoff])
            .map_err(|e| format!("清理会话失败: {e}"))
    }

    // ---------- 已确认存在的远端目录 ----------

    pub fn load_dirs(&self, job_id: i64) -> Result<Vec<String>, String> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT path FROM remote_dirs WHERE job_id = ?1")
            .map_err(|e| format!("查询远端目录失败: {e}"))?;
        let rows = stmt
            .query_map(params![job_id], |r| r.get::<_, String>(0))
            .map_err(|e| format!("查询远端目录失败: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取远端目录失败: {e}"))
    }

    pub fn add_dir(&self, job_id: i64, path: &str) -> Result<(), String> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO remote_dirs(job_id, path) VALUES(?1, ?2)",
            params![job_id, path],
        )
        .map_err(|e| format!("记录远端目录失败: {e}"))?;
        Ok(())
    }

    pub fn clear_dirs(&self, job_id: i64) -> Result<(), String> {
        let conn = self.lock();
        conn.execute("DELETE FROM remote_dirs WHERE job_id = ?1", params![job_id])
            .map_err(|e| format!("清空远端目录失败: {e}"))?;
        Ok(())
    }
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn 目的地增删改查() {
        let (_dir, store) = temp_store();
        let id = store
            .insert_destination("nas-cd2", "clouddrive2", r#"{"address":"http://x:19798","root":"/115open"}"#, Some("sealed"))
            .unwrap();
        let row = store.get_destination(id).unwrap().unwrap();
        assert_eq!(row.name, "nas-cd2");
        assert_eq!(row.secret_sealed.as_deref(), Some("sealed"));
        // 不带 secret 的更新保留旧密文
        store.update_destination(id, "nas-cd2", row.config_json.as_str(), None).unwrap();
        assert_eq!(store.get_destination(id).unwrap().unwrap().secret_sealed.as_deref(), Some("sealed"));
        store.delete_destination(id).unwrap();
        assert!(store.get_destination(id).unwrap().is_none());
    }

    #[test]
    fn 有任务引用时不许删目的地() {
        let (_dir, store) = temp_store();
        let id = store.insert_destination("d", "clouddrive2", "{}", None).unwrap();
        store
            .insert_job("j", true, "mirror", id, "/115open/a", None, None, "{}", None)
            .unwrap();
        assert!(store.delete_destination(id).is_err());
    }

    #[test]
    fn 索引覆盖写幂等() {
        let (_dir, store) = temp_store();
        let mut row = FileRow {
            job_id: 1,
            rel: "a/b.mkv".into(),
            size: 10,
            mtime_ns: 123,
            sha1: Some("aa".into()),
            remote_path: "/115open/x/a/b.mkv".into(),
            updated_at: now(),
        };
        store.upsert_file(&row).unwrap();
        row.size = 20;
        row.sha1 = Some("bb".into());
        store.upsert_file(&row).unwrap();
        let all = store.load_files(1).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].size, 20);
        assert_eq!(all[0].sha1.as_deref(), Some("bb"));
    }

    #[test]
    fn 中断记录启动时被标失败() {
        let (_dir, store) = temp_store();
        let id = store.start_run(1, "manual", "/tmp/x.log").unwrap();
        assert_eq!(store.fail_stale_runs().unwrap(), 1);
        let run = store.run_by_id(id).unwrap().unwrap();
        assert_eq!(run.status, "failed");
    }

    #[test]
    fn 策略默认与覆盖() {
        let (_dir, store) = temp_store();
        assert_eq!(store.policy().file_gap_ms, 300);
        let mut p = store.policy();
        p.poll_ms = 4000;
        store.set_policy(&p).unwrap();
        assert_eq!(store.policy().poll_ms, 4000);
    }
}
