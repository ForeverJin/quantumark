// 任务运行需要的公共设施：运行上下文（日志/统计/取消）、本地扫描、
// 远端目录缓存。

pub mod archive;
#[cfg(test)]
pub mod fake;
pub mod mirror;
pub mod mysql;
pub mod pipeline;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use globset::GlobSet;
use serde::Serialize;

use crate::destination::{DestError, Destination, join_path, split_path};
use crate::model::{FileRow, Policy, RunStats};
use crate::store::{Store, now};

/// 运行中止的原因。
#[derive(Debug, Clone)]
pub enum RunAbort {
    /// 用户点了停止
    Cancelled,
    /// 撞上风控/限流，整轮必须停
    RiskControl(String),
    /// 连续失败太多
    TooManyFailures(u32),
    /// 配置/环境问题，直接判失败
    Error(String),
}

impl RunAbort {
    /// 转成 (运行状态, 错误信息)。
    pub fn status_message(&self) -> (&'static str, String) {
        match self {
            RunAbort::Cancelled => ("cancelled", "用户停止".to_string()),
            RunAbort::RiskControl(m) => ("failed", format!("触发风控/限流，已停止并进入冷却: {m}")),
            RunAbort::TooManyFailures(n) => ("failed", format!("连续失败 {n} 次，已停止本轮")),
            RunAbort::Error(m) => ("failed", m.clone()),
        }
    }
}

/// 一次运行的上下文。克隆后可以丢进任务里用。
#[derive(Clone)]
pub struct RunCtx {
    inner: Arc<Inner>,
}

struct Inner {
    run_id: i64,
    job_name: String,
    policy: Policy,
    cancel: Arc<AtomicBool>,
    stats: Mutex<RunStats>,
    log: Mutex<std::fs::File>,
}

impl RunCtx {
    pub fn new(
        run_id: i64,
        job_name: &str,
        policy: Policy,
        cancel: Arc<AtomicBool>,
        log_file: std::fs::File,
    ) -> RunCtx {
        RunCtx {
            inner: Arc::new(Inner {
                run_id,
                job_name: job_name.to_string(),
                policy,
                cancel,
                stats: Mutex::new(RunStats::default()),
                log: Mutex::new(log_file),
            }),
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.inner.policy
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.inner.cancel.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancel.load(Ordering::Relaxed)
    }

    /// 写一行日志：落到本次运行的日志文件里。
    pub fn log(&self, level: &str, msg: &str) {
        let line = format!(
            "{} [{}] {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            level,
            msg
        );
        if let Ok(mut f) = self.inner.log.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
        // 同时打到标准错误，容器日志里也能看到
        eprint!("[{}#{}] {}", self.inner.job_name, self.inner.run_id, line);
    }

    pub fn info(&self, msg: impl AsRef<str>) {
        self.log("INFO", msg.as_ref());
    }

    pub fn warn(&self, msg: impl AsRef<str>) {
        self.log("WARN", msg.as_ref());
    }

    pub fn error(&self, msg: impl AsRef<str>) {
        self.log("ERROR", msg.as_ref());
    }

    /// 更新统计（闭包形式，避免外部直接持有锁）。
    pub fn with_stats(&self, f: impl FnOnce(&mut RunStats)) {
        if let Ok(mut s) = self.inner.stats.lock() {
            f(&mut s);
        }
    }

    pub fn stats_snapshot(&self) -> RunStats {
        self.inner.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// 每处理一个对象前调用：检查是否被取消。
    pub fn checkpoint(&self) -> Result<(), RunAbort> {
        if self.is_cancelled() {
            Err(RunAbort::Cancelled)
        } else {
            Ok(())
        }
    }

    /// 连续失败后是否该中止。
    pub fn register_failure(&self, streak: u32) -> Result<(), RunAbort> {
        if streak >= self.inner.policy.max_failures {
            Err(RunAbort::TooManyFailures(streak))
        } else {
            Ok(())
        }
    }
}

/// 本地扫描到的一个文件。
#[derive(Debug, Clone, Serialize)]
pub struct LocalFile {
    /// 相对源目录的路径，用 / 分隔
    pub rel: String,
    pub size: i64,
    pub mtime_ns: i64,
    /// 本机绝对路径
    pub abs: String,
}

/// 扫描结果。
#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<LocalFile>,
    pub dirs: u64,
    pub skipped_symlinks: u64,
    pub skipped_excluded: u64,
    pub unreadable: u64,
}

/// 递归扫描源目录。
///
/// 规则：
///   - 不跟随符号链接（避免把别的目录结构带进来）
///   - 排除规则同时匹配相对路径和文件名
///   - 包含规则为空表示全要
///   - 每扫描若干项检查一次取消
pub fn scan_local(
    root: &Path,
    include: &GlobSet,
    exclude: &GlobSet,
    cancel: &AtomicBool,
) -> Result<ScanResult, String> {
    if !root.is_dir() {
        return Err(format!("源目录不存在或不是目录: {}", root.display()));
    }
    let mut out = ScanResult::default();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    let mut tick: u64 = 0;

    while let Some((dir, rel_dir)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(out);
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                out.unreadable += 1;
                let _ = e;
                continue;
            }
        };
        out.dirs += 1;
        for entry in entries.flatten() {
            tick += 1;
            if tick % 500 == 0 && cancel.load(Ordering::Relaxed) {
                return Ok(out);
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = if rel_dir.is_empty() {
                name.clone()
            } else {
                format!("{rel_dir}/{name}")
            };
            let abs = entry.path();
            let meta = match std::fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(_) => {
                    out.unreadable += 1;
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                out.skipped_symlinks += 1;
                continue;
            }
            if exclude.is_match(&rel) || exclude.is_match(&name) {
                out.skipped_excluded += 1;
                continue;
            }
            if meta.is_dir() {
                stack.push((abs, rel));
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            if !include.is_empty() && !(include.is_match(&rel) || include.is_match(&name)) {
                out.skipped_excluded += 1;
                continue;
            }
            let mtime_ns = mtime_ns(&meta);
            out.files.push(LocalFile {
                rel,
                size: meta.len() as i64,
                mtime_ns,
                abs: abs.to_string_lossy().to_string(),
            });
        }
    }
    out.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let secs = meta.mtime();
        let nanos = meta.mtime_nsec();
        secs.saturating_mul(1_000_000_000).saturating_add(nanos)
    }
    #[cfg(not(unix))]
    {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0)
    }
}

/// 远端目录缓存：内存 + 数据库，避免重启后为每个目录再查一次远端。
///
/// 说明：每一次「查目录」在 115 侧都是要计配额的（见项目文档里的 770004 风控），
/// 所以确认过的目录要持久化。
pub struct RemoteDirCache {
    job_id: i64,
    known: HashSet<String>,
}

impl RemoteDirCache {
    pub fn load(store: &Store, job_id: i64) -> Result<RemoteDirCache, String> {
        let known: HashSet<String> = store.load_dirs(job_id)?.into_iter().collect();
        Ok(RemoteDirCache { job_id, known })
    }

    pub fn contains(&self, path: &str) -> bool {
        self.known.contains(path)
    }

    pub fn insert(&mut self, store: &Store, path: &str) -> Result<(), String> {
        if self.known.insert(path.to_string()) {
            store.add_dir(self.job_id, path)?;
        }
        Ok(())
    }
}

/// 生成索引行。索引不再计算/保存 SHA1。
pub fn index_row(job_id: i64, rel: &str, size: i64, mtime_ns: i64, remote_path: &str) -> FileRow {
    FileRow {
        job_id,
        rel: rel.to_string(),
        size,
        mtime_ns,
        sha1: None,
        remote_path: remote_path.to_string(),
        updated_at: now(),
    }
}

/// 拼远端文件路径。
pub fn remote_path_of(remote_dir: &str, rel: &str) -> String {
    join_path(remote_dir, rel)
}

// ---------- 各任务共用的步骤 ----------

/// 清理上次中断留下的 .bmpart 临时文件。
pub async fn cleanup_pending(
    ctx: &RunCtx,
    dest: &dyn Destination,
    store: &Store,
    job_id: i64,
) -> Result<(), RunAbort> {
    let rows = store.list_pending(job_id).map_err(RunAbort::Error)?;
    if rows.is_empty() {
        return Ok(());
    }
    ctx.info(format!("清理 {} 个上次中断留下的临时文件", rows.len()));
    for row in rows {
        ctx.checkpoint()?;
        match dest.delete(&row.remote_temp_path).await {
            Ok(()) | Err(DestError::NotFound(_)) => {
                store.remove_pending(row.id).ok();
            }
            Err(DestError::RiskControl(m)) => return Err(RunAbort::RiskControl(m)),
            Err(e) => ctx.warn(format!("清理 {} 失败: {e}", row.remote_temp_path)),
        }
        tokio::time::sleep(std::time::Duration::from_millis(ctx.policy().delete_gap_ms)).await;
    }
    Ok(())
}

/// 确保远端目录存在（带持久化缓存，避免重启后重复查询）。
pub async fn ensure_dir_cached(
    _ctx: &RunCtx,
    dest: &dyn Destination,
    store: &Store,
    dirs: &mut RemoteDirCache,
    path: &str,
) -> Result<(), RunAbort> {
    if dirs.contains(path) {
        return Ok(());
    }
    match dest.ensure_dir(path).await {
        Ok(()) => dirs.insert(store, path).map_err(RunAbort::Error),
        Err(DestError::RiskControl(m)) => Err(RunAbort::RiskControl(m)),
        Err(e) => Err(RunAbort::Error(format!("创建远端目录 {path} 失败: {e}"))),
    }
}

/// 保留份数：只处理同一组（同一个远端目录）下的归档。
pub async fn apply_retention(
    ctx: &RunCtx,
    dest: &dyn Destination,
    store: &Store,
    job_id: i64,
    group_dir: &str,
    keep: i64,
) -> Result<(), RunAbort> {
    let keep = keep.max(1) as usize;
    let mut rows: Vec<_> = store
        .list_archives(job_id)
        .map_err(RunAbort::Error)?
        .into_iter()
        .filter(|r| split_path(&r.remote_path).0 == group_dir)
        .collect();
    if rows.len() <= keep {
        return Ok(());
    }
    rows.sort_by(|a, b| b.id.cmp(&a.id));
    let stale: Vec<_> = rows.into_iter().skip(keep).collect();
    ctx.info(format!(
        "保留最近 {keep} 份，删除 {} 份旧归档",
        stale.len()
    ));
    let mut streak = 0u32;
    for row in stale {
        ctx.checkpoint()?;
        match dest.delete(&row.remote_path).await {
            Ok(()) | Err(DestError::NotFound(_)) => {
                store.remove_archive(&row.remote_path).map_err(RunAbort::Error)?;
                ctx.with_stats(|s| s.deleted += 1);
                ctx.info(format!("已删除旧归档 {}", row.remote_path));
            }
            Err(DestError::RiskControl(m)) => return Err(RunAbort::RiskControl(m)),
            Err(e) => {
                streak = register_failure(ctx, &row.remote_path, &e, streak)?;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(ctx.policy().delete_gap_ms)).await;
    }
    Ok(())
}

/// 统计一次失败；达到阈值就让整轮中止。
pub fn register_failure(
    ctx: &RunCtx,
    what: &str,
    err: &DestError,
    streak: u32,
) -> Result<u32, RunAbort> {
    if err.abort_run() {
        return Err(RunAbort::RiskControl(err.message().to_string()));
    }
    ctx.with_stats(|s| s.failed += 1);
    ctx.error(format!("{what}: {err}"));
    let next = streak + 1;
    ctx.register_failure(next)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::{Glob, GlobSetBuilder};

    fn build(patterns: &[&str]) -> GlobSet {
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            b.add(Glob::new(p).unwrap());
        }
        b.build().unwrap()
    }

    #[test]
    fn 扫描遵守排除规则且排序稳定() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::create_dir_all(dir.path().join("@eaDir")).unwrap();
        std::fs::write(dir.path().join("b.mkv"), b"bb").unwrap();
        std::fs::write(dir.path().join("a.mkv"), b"a").unwrap();
        std::fs::write(dir.path().join("sub/c.mkv"), b"ccc").unwrap();
        std::fs::write(dir.path().join("sub/skip.tmp"), b"x").unwrap();
        std::fs::write(dir.path().join("@eaDir/thumb.jpg"), b"x").unwrap();

        let exclude = build(&["@eaDir", "*.tmp"]);
        let include = build(&[]);
        let cancel = AtomicBool::new(false);
        let res = scan_local(dir.path(), &include, &exclude, &cancel).unwrap();
        let rels: Vec<_> = res.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["a.mkv", "b.mkv", "sub/c.mkv"]);
        assert_eq!(res.skipped_excluded, 2);
    }

    #[test]
    fn 包含规则限制范围() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mkv"), b"a").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        let res = scan_local(dir.path(), &build(&["*.mkv"]), &build(&[]), &AtomicBool::new(false)).unwrap();
        assert_eq!(res.files.len(), 1);
        assert_eq!(res.files[0].rel, "a.mkv");
    }

    #[test]
    fn 取消后尽早返回() {
        let dir = tempfile::tempdir().unwrap();
        let res = scan_local(dir.path(), &build(&[]), &build(&[]), &AtomicBool::new(true));
        assert!(res.is_ok());
    }

    #[test]
    fn 远端路径拼接() {
        assert_eq!(remote_path_of("/115open/backup", "a/b.mkv"), "/115open/backup/a/b.mkv");
    }
}
