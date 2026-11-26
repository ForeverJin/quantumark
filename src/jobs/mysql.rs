// MySQL / MariaDB 备份任务。
//
// 三种方式（任务参数里的 mode）：
//   dump    连数据库做逻辑转储（默认）：mariadb-dump → zstd →（age）→ 上传到 CD2。
//           一致性由 --single-transaction 保证，不需要停库、不需要访问数据目录。
//   archive 把数据目录打成一个 tar.zst 包（文件级；建议停库或用文件系统快照）。
//   mirror  按文件增量同步数据目录（文件级）。
//
// 无论哪种方式，写入都只经 CD2 的接口，由 CD2 负责传到 115。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::pipeline::{
    Pipeline, StageSpec, find_dump_program, find_mysql_client, program_available,
};
use super::{
    RemoteDirCache, RunAbort, RunCtx, apply_retention, archive, cleanup_pending,
    ensure_dir_cached, mirror, register_failure,
};
use crate::destination::{DestError, Destination, UploadRequest, join_path};
use crate::model::{ArchiveOptions, JobSecret, MirrorOptions, MysqlOptions};
use crate::store::Store;

/// 系统库，dump 方式默认不导出。
const SYSTEM_DBS: [&str; 4] = ["information_schema", "performance_schema", "mysql", "sys"];

pub async fn run(
    ctx: &RunCtx,
    dest: &dyn Destination,
    store: &Store,
    job_id: i64,
    remote_dir: &str,
    opts: &MysqlOptions,
    secret: &JobSecret,
    keep: Option<i64>,
) -> Result<(), RunAbort> {
    match opts.mode.trim().to_lowercase().as_str() {
        "" | "dump" => run_dump(ctx, dest, store, job_id, remote_dir, opts, secret, keep).await,
        "archive" => run_files(ctx, dest, store, job_id, remote_dir, opts, keep, true).await,
        "mirror" => run_files(ctx, dest, store, job_id, remote_dir, opts, keep, false).await,
        other => Err(RunAbort::Error(format!(
            "备份方式只能是 dump（连库导出）/ archive（打包数据目录）/ mirror（同步数据目录），当前: {other}"
        ))),
    }
}

// ---------- dump：连数据库导出 ----------

async fn run_dump(
    ctx: &RunCtx,
    dest: &dyn Destination,
    store: &Store,
    job_id: i64,
    remote_dir: &str,
    opts: &MysqlOptions,
    secret: &JobSecret,
    keep: Option<i64>,
) -> Result<(), RunAbort> {
    let policy = ctx.policy().clone();
    let password = secret.mysql_password.clone().unwrap_or_default();

    if opts.host.trim().is_empty() {
        return Err(RunAbort::Error("数据库地址不能为空".to_string()));
    }
    if opts.user.trim().is_empty() {
        return Err(RunAbort::Error("数据库用户名不能为空".to_string()));
    }
    let recipient = normalize_recipient(&opts.age_recipient)?;

    let dump_program = find_dump_program().await.map_err(RunAbort::Error)?;
    if !program_available("zstd").await {
        return Err(RunAbort::Error("容器里找不到 zstd".to_string()));
    }
    if recipient.is_some() && !program_available("age").await {
        return Err(RunAbort::Error("选择了加密，但容器里找不到 age".to_string()));
    }

    // 要备份哪些库
    let databases = if opts.databases.is_empty() {
        ctx.info("未指定库名，自动列出所有非系统库");
        discover_databases(ctx, opts, &password).await?
    } else {
        for db in &opts.databases {
            if !valid_db_name(db) {
                return Err(RunAbort::Error(format!("库名不合法: {db}")));
            }
        }
        opts.databases.clone()
    };
    if databases.is_empty() {
        return Err(RunAbort::Error("没有可备份的数据库".to_string()));
    }
    ctx.info(format!(
        "准备导出 {} 个库：{}",
        databases.len(),
        databases.join(", ")
    ));

    cleanup_pending(ctx, dest, store, job_id).await?;
    let mut dirs = RemoteDirCache::load(store, job_id).map_err(RunAbort::Error)?;
    ensure_dir_cached(ctx, dest, store, &mut dirs, remote_dir).await?;

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let ext = if recipient.is_some() { ".zst.age" } else { ".zst" };
    let mut streak = 0u32;

    for db in &databases {
        ctx.checkpoint()?;
        let db_dir = join_path(remote_dir, db);
        ensure_dir_cached(ctx, dest, store, &mut dirs, &db_dir).await?;

        let file_name = format!("{db}-{ts}.sql{ext}");
        let dst = join_path(&db_dir, &file_name);
        let temp = join_path(&db_dir, &format!("{file_name}.bmpart"));
        let pending_id = store.add_pending(job_id, &temp).map_err(RunAbort::Error)?;

        let mut dump_args = vec![
            "-h".to_string(),
            opts.host.trim().to_string(),
            "-P".to_string(),
            opts.port.to_string(),
            "-u".to_string(),
            opts.user.trim().to_string(),
            "--single-transaction".to_string(),
            "--quick".to_string(),
            "--routines".to_string(),
            "--triggers".to_string(),
            "--events".to_string(),
            "--hex-blob".to_string(),
            "--no-tablespaces".to_string(),
            "--default-character-set=utf8mb4".to_string(),
        ];
        dump_args.extend(opts.extra_args.iter().cloned());
        dump_args.push(db.clone());

        let mut stages =
            vec![StageSpec::new(&dump_program, dump_args).with_env("MYSQL_PWD", password.clone())];
        stages.push(StageSpec::new(
            "zstd",
            vec![
                format!("-{}", opts.zstd_level.clamp(1, 19)),
                "-T2".to_string(),
                "-q".to_string(),
            ],
        ));
        if let Some(r) = &recipient {
            stages.push(StageSpec::new("age", vec!["-r".to_string(), r.clone()]));
        }

        ctx.info(format!("开始导出 {db} → {dst}"));
        let mut pipe = Pipeline::start(stages).map_err(RunAbort::Error)?;
        let reader = pipe.take_reader();

        let progress = Arc::new(AtomicU64::new(0));
        let ticker = spawn_progress_logger(ctx, progress.clone(), db.clone());

        let upload = dest
            .upload(UploadRequest {
                dst_path: &dst,
                reader,
                expected_len: None,
                expect_existing: false,
                progress,
                cancel: ctx.cancel_flag(),
            })
            .await;
        ticker.abort();

        let outcome = match upload {
            Ok(out) => match pipe.finish().await {
                Ok(()) => Ok(out),
                Err(e) => {
                    ctx.error(format!("{db}: 导出进程异常：{e}"));
                    let _ = dest.delete(&dst).await;
                    Err(DestError::Other(format!("导出失败: {e}")))
                }
            },
            Err(e) => {
                pipe.kill();
                Err(e)
            }
        };

        match outcome {
            Ok(out) => {
                store.remove_pending(pending_id).ok();
                store
                    .add_archive(job_id, &dst, out.size, None)
                    .map_err(RunAbort::Error)?;
                ctx.with_stats(|s| {
                    s.uploaded += 1;
                    s.uploaded_bytes += out.size.max(0) as u64;
                });
                ctx.info(format!(
                    "{db}: 完成（{}）",
                    mirror::human(out.size.max(0) as u64)
                ));
                if let Some(keep) = keep {
                    apply_retention(ctx, dest, store, job_id, &db_dir, keep).await?;
                }
                streak = 0;
            }
            Err(e) => {
                // 单个库失败不影响其它库，除非是风控
                streak = register_failure(ctx, db, &e, streak)?;
            }
        }

        tokio::time::sleep(Duration::from_millis(policy.file_gap_ms)).await;
    }

    ctx.info(format!(
        "恢复方式：age -d -i <私钥> <归档> | zstd -d | {} -h {} -P {} -u {} -p <库名>",
        find_mysql_client().await.unwrap_or_else(|_| "mariadb".to_string()),
        opts.host,
        opts.port,
        opts.user
    ));
    Ok(())
}

/// 列出所有非系统库（连数据库查）。
async fn discover_databases(
    ctx: &RunCtx,
    opts: &MysqlOptions,
    password: &str,
) -> Result<Vec<String>, RunAbort> {
    let client = find_mysql_client().await.map_err(RunAbort::Error)?;
    let output = tokio::process::Command::new(&client)
        .args([
            "-h",
            opts.host.trim(),
            "-P",
            &opts.port.to_string(),
            "-u",
            opts.user.trim(),
            "-N",
            "-e",
            "SHOW DATABASES",
        ])
        .env("MYSQL_PWD", password)
        .output()
        .await
        .map_err(|e| RunAbort::Error(format!("连接数据库失败（{client}）: {e}")))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(RunAbort::Error(format!(
            "查询数据库列表失败: {}",
            err.trim().replace('\n', " ")
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut dbs: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !SYSTEM_DBS.contains(&s.to_lowercase().as_str()))
        .filter(|s| valid_db_name(s))
        .map(str::to_string)
        .collect();
    dbs.sort();
    dbs.dedup();
    ctx.info(format!("发现 {} 个非系统库", dbs.len()));
    Ok(dbs)
}

// ---------- archive / mirror：备份数据目录 ----------

async fn run_files(
    ctx: &RunCtx,
    dest: &dyn Destination,
    store: &Store,
    job_id: i64,
    remote_dir: &str,
    opts: &MysqlOptions,
    keep: Option<i64>,
    as_archive: bool,
) -> Result<(), RunAbort> {
    let source = opts.source.trim().to_string();
    if source.is_empty() {
        return Err(RunAbort::Error(
            "数据目录不能为空（这种备份方式需要把数据目录挂进来）".to_string(),
        ));
    }
    let path = Path::new(&source);
    if !path.is_dir() {
        return Err(RunAbort::Error(format!(
            "数据目录不存在或不是目录: {source}（容器内路径）"
        )));
    }
    if !looks_like_datadir(path) {
        ctx.warn(format!(
            "{source} 里没有 ibdata1 / mysql / ib_logfile0 等特征文件，确认选的是 MySQL 数据目录吗？仍按普通目录备份"
        ));
    }
    let markers = running_markers(path);
    if !markers.is_empty() {
        ctx.warn(format!(
            "检测到 {}，数据库可能正在运行：运行中拷贝数据目录可能得到不一致的副本。建议改用「连数据库导出」，或停库/用快照。",
            markers.join(" / ")
        ));
    }

    let excludes = merged_excludes(&opts.exclude);
    if as_archive {
        let archive_opts = ArchiveOptions {
            source,
            exclude: excludes,
            zstd_level: opts.zstd_level,
            age_recipient: opts.age_recipient.clone(),
        };
        archive::run(ctx, dest, store, job_id, remote_dir, &archive_opts, keep).await
    } else {
        let mirror_opts = MirrorOptions {
            source,
            include: Vec::new(),
            exclude: excludes,
            // 数据目录不做同步删除：删表留下的云端旧文件留着更安全；
            // 也不删本地（数据库文件不能删）
            sync_delete: false,
            skip_adopt: false,
            delete_local: false,
            local_trash: None,
        };
        mirror::run(ctx, dest, store, job_id, remote_dir, &mirror_opts).await
    }
}

/// 数据目录里不该备份的临时文件。
pub fn default_excludes() -> Vec<String> {
    [
        "ibtmp1",
        "#innodb_temp",
        "#innodb_temp*",
        "*.pid",
        "*.sock",
        "*.err",
        "*.tmp",
        "*.part",
        "*.bmpart",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn merged_excludes(user: &[String]) -> Vec<String> {
    let mut out = default_excludes();
    for rule in user {
        let rule = rule.trim();
        if !rule.is_empty() && !out.iter().any(|x| x == rule) {
            out.push(rule.to_string());
        }
    }
    out
}

fn looks_like_datadir(path: &Path) -> bool {
    ["ibdata1", "mysql", "ib_logfile0", "aria_log_control", "#innodb_redo"]
        .iter()
        .any(|name| path.join(name).exists())
}

fn running_markers(path: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(path) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".sock") || name.ends_with(".pid") {
            found.push(name);
        }
    }
    found.sort();
    found
}

fn normalize_recipient(raw: &Option<String>) -> Result<Option<String>, RunAbort> {
    let recipient = raw
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(r) = &recipient {
        if !r.starts_with("age1") {
            return Err(RunAbort::Error(
                "age 公钥应以 age1 开头（用 age-keygen 生成）".to_string(),
            ));
        }
    }
    Ok(recipient)
}

fn valid_db_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '$')
}

fn spawn_progress_logger(
    ctx: &RunCtx,
    progress: Arc<AtomicU64>,
    label: String,
) -> tokio::task::JoinHandle<()> {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            if ctx.is_cancelled() {
                return;
            }
            let sent = progress.load(Ordering::Relaxed);
            if sent > 0 {
                ctx.info(format!("{label}: 已发送 {}", mirror::human(sent)));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::fake::{FakeDest, fast_policy, test_ctx};
    use crate::store::Store;

    fn sample_datadir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("mysql")).unwrap();
        std::fs::create_dir_all(dir.path().join("shop")).unwrap();
        std::fs::write(dir.path().join("ibdata1"), b"ib").unwrap();
        std::fs::write(dir.path().join("ib_logfile0"), b"redo").unwrap();
        std::fs::write(dir.path().join("mysql/global.ibd"), b"g").unwrap();
        std::fs::write(dir.path().join("shop/orders.ibd"), b"orders").unwrap();
        std::fs::write(dir.path().join("ibtmp1"), b"tmp").unwrap();
        std::fs::write(dir.path().join("mysqld.pid"), b"1").unwrap();
        std::fs::write(dir.path().join("mysqld.sock"), b"").unwrap();
        dir
    }

    fn files_options(dir: &std::path::Path, mode: &str) -> MysqlOptions {
        MysqlOptions {
            mode: mode.to_string(),
            host: String::new(),
            port: 3306,
            user: String::new(),
            databases: vec![],
            extra_args: vec![],
            source: dir.to_string_lossy().to_string(),
            exclude: vec![],
            zstd_level: 3,
            age_recipient: None,
        }
    }

    async fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn 默认排除包含数据库临时文件() {
        let ex = default_excludes();
        assert!(ex.contains(&"ibtmp1".to_string()));
        assert!(ex.contains(&"#innodb_temp".to_string()));
        assert!(ex.contains(&"*.pid".to_string()));
        assert!(!ex.iter().any(|x| x.contains("ib_logfile")), "redo 日志不能排除");
    }

    #[test]
    fn 用户排除规则去重合并() {
        let merged = merged_excludes(&["*.bak".to_string(), "ibtmp1".to_string(), " ".to_string()]);
        assert_eq!(merged.iter().filter(|x| *x == "ibtmp1").count(), 1);
        assert!(merged.contains(&"*.bak".to_string()));
    }

    #[test]
    fn 库名校验() {
        assert!(valid_db_name("inventory_db"));
        assert!(valid_db_name("ominscope"));
        assert!(!valid_db_name(""));
        assert!(!valid_db_name("bad name"));
        assert!(!valid_db_name("../etc"));
        assert!(!valid_db_name("a;drop"));
    }

    #[test]
    fn 识别数据目录与运行标记() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!looks_like_datadir(dir.path()));
        std::fs::write(dir.path().join("ibdata1"), b"x").unwrap();
        assert!(looks_like_datadir(dir.path()));
        assert!(running_markers(dir.path()).is_empty());
        std::fs::write(dir.path().join("mysqld.sock"), b"").unwrap();
        assert_eq!(running_markers(dir.path()).len(), 1);
    }

    #[tokio::test]
    async fn dump_方式缺地址要报错() {
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        let (ctx, _log) = test_ctx(fast_policy());
        let mut opts = files_options(std::path::Path::new("/tmp"), "dump");
        opts.user = "admin".to_string();
        let result = run(
            &ctx,
            &dest,
            &store,
            1,
            "/115open/b/mysql",
            &opts,
            &JobSecret::default(),
            None,
        )
        .await;
        assert!(matches!(result, Err(RunAbort::Error(_))), "实际: {result:?}");
    }

    #[tokio::test]
    async fn 文件方式镜像会排除临时文件() {
        let dir = sample_datadir();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        let (ctx, _log) = test_ctx(fast_policy());
        run(
            &ctx,
            &dest,
            &store,
            1,
            "/115open/backup/mysql",
            &files_options(dir.path(), "mirror"),
            &JobSecret::default(),
            None,
        )
        .await
        .unwrap();
        assert!(dest.exists("/115open/backup/mysql/ibdata1"));
        assert!(dest.exists("/115open/backup/mysql/shop/orders.ibd"));
        assert!(!dest.exists("/115open/backup/mysql/ibtmp1"));
        assert!(!dest.exists("/115open/backup/mysql/mysqld.pid"));
    }

    #[tokio::test]
    async fn 文件方式归档并按份数轮转() {
        if !std::path::Path::new("/usr/bin/zstd").exists() {
            return;
        }
        let dir = sample_datadir();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        let opts = files_options(dir.path(), "archive");

        let (ctx1, _l1) = test_ctx(fast_policy());
        run(
            &ctx1,
            &dest,
            &store,
            1,
            "/115open/backup/mysql",
            &opts,
            &JobSecret::default(),
            Some(1),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let (ctx2, _l2) = test_ctx(fast_policy());
        run(
            &ctx2,
            &dest,
            &store,
            1,
            "/115open/backup/mysql",
            &opts,
            &JobSecret::default(),
            Some(1),
        )
        .await
        .unwrap();
        assert_eq!(dest.upload_count(), 2);
        assert_eq!(store.list_archives(1).unwrap().len(), 1);
        assert!(dest.delete_count() >= 1);
    }

    #[tokio::test]
    async fn 未知方式要报错() {
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        let (ctx, _log) = test_ctx(fast_policy());
        let opts = files_options(std::path::Path::new("/tmp"), "whatever");
        let result = run(
            &ctx,
            &dest,
            &store,
            1,
            "/115open/b/mysql",
            &opts,
            &JobSecret::default(),
            None,
        )
        .await;
        assert!(matches!(result, Err(RunAbort::Error(_))));
    }
}
