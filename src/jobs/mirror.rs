// 目录镜像：把本地目录增量同步到 115。
//
// 关键约束（来自项目文档里的风控记录）：
//   - 除「索引为空时的首次认领」外，不读远端目录；变化判断靠本地索引。
//   - 串行上传、文件之间留最小间隔；连续失败到阈值就整轮停。
//   - 同步删除只作用于索引里记录过、且位于本任务备份目录下的路径。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};

use super::{
    LocalFile, RemoteDirCache, RunAbort, RunCtx, cleanup_pending, ensure_dir_cached,
    index_row, register_failure, remote_path_of, scan_local,
};
use crate::destination::{
    DestError, Destination, RemoteEntry, UploadRequest, join_path, split_path,
};
use crate::model::{FileRow, MirrorOptions};
use crate::store::Store;

pub async fn run(
    ctx: &RunCtx,
    dest: &dyn Destination,
    store: &Store,
    job_id: i64,
    remote_dir: &str,
    opts: &MirrorOptions,
) -> Result<(), RunAbort> {
    let policy = ctx.policy().clone();
    if opts.delete_local && opts.sync_delete {
        return Err(RunAbort::Error(
            "「上传后删除本地」和「同步删除云端」不能同时开启（一个删本地、一个删云端）".to_string(),
        ));
    }
    let include = compile(&opts.include).map_err(RunAbort::Error)?;
    let exclude = compile(&opts.exclude).map_err(RunAbort::Error)?;

    // 1. 扫本地
    ctx.info(format!("开始扫描源目录 {}", opts.source));
    let root = Path::new(&opts.source).to_path_buf();
    let cancel = ctx.cancel_flag();
    let scan = tokio::task::spawn_blocking(move || scan_local(&root, &include, &exclude, &cancel))
        .await
        .map_err(|e| RunAbort::Error(format!("扫描任务异常: {e}")))?
        .map_err(RunAbort::Error)?;
    ctx.checkpoint()?;

    ctx.info(format!(
        "扫描完成：{} 个文件、{} 个目录（跳过符号链接 {}，按规则排除 {}，无法读取 {}）",
        scan.files.len(),
        scan.dirs,
        scan.skipped_symlinks,
        scan.skipped_excluded,
        scan.unreadable
    ));
    if scan.files.is_empty() {
        ctx.warn("源目录里没有可备份的文件，本轮结束");
        return Ok(());
    }
    ctx.with_stats(|s| s.scanned = scan.files.len() as u64);

    // 2. 和索引比对
    let index: HashMap<String, FileRow> = store
        .load_files(job_id)
        .map_err(RunAbort::Error)?
        .into_iter()
        .map(|r| (r.rel.clone(), r))
        .collect();

    let mut candidates: Vec<LocalFile> = Vec::new();
    let mut skipped: u64 = 0;
    // 本次不需要上传、但索引里已有确认记录的文件：开了删本地时可以直接清
    let mut already_in_cloud: Vec<LocalFile> = Vec::new();
    for f in &scan.files {
        let expected = remote_path_of(remote_dir, &f.rel);
        match index.get(&f.rel) {
            Some(row) if row.size == f.size && row.mtime_ns == f.mtime_ns && row.remote_path == expected => {
                skipped += 1;
                if opts.delete_local {
                    already_in_cloud.push(f.clone());
                }
            }
            _ => candidates.push(f.clone()),
        }
    }
    ctx.with_stats(|s| {
        s.skipped = skipped;
        s.candidates = candidates.len() as u64;
    });
    ctx.info(format!(
        "需要处理 {} 个文件（未变化 {} 个）",
        candidates.len(),
        skipped
    ));

    // 3. 清理上次中断留下的临时文件
    cleanup_pending(ctx, dest, store, job_id).await?;

    // 4. 首次认领（索引为空且远端已有内容时）
    let first_run = index.is_empty() && !opts.skip_adopt;
    let mut replaced: HashSet<String> = HashSet::new();
    if first_run && !candidates.is_empty() {
        ctx.info("索引为空：先读一次云端备份目录，按大小认领文件（只做这一次，之后不再读远端目录）");
        match adopt(ctx, dest, remote_dir).await {
            Ok(remote) => {
                let mut remain = Vec::new();
                for f in candidates.into_iter() {
                    let target = remote_path_of(remote_dir, &f.rel);
                    match remote.get(&f.rel) {
                        Some(entry) if !entry.is_dir && entry.size == f.size => {
                            // 不计算本地 SHA1，按大小认领；同大小但内容不同的文件也会被认领
                            store
                                .upsert_file(&index_row(
                                    job_id,
                                    &f.rel,
                                    f.size,
                                    f.mtime_ns,
                                    &target,
                                ))
                                .map_err(RunAbort::Error)?;
                            ctx.with_stats(|s| s.adopted += 1);
                        }
                        Some(_) => {
                            // 远端同名但大小不同
                            replaced.insert(f.rel.clone());
                            remain.push(f);
                        }
                        None => remain.push(f),
                    }
                }
                candidates = remain;
                ctx.info(format!(
                    "认领完成：{} 个文件已在云端，剩余 {} 个需要上传",
                    ctx.stats_snapshot().adopted,
                    candidates.len()
                ));
            }
            Err(DestError::RiskControl(m)) => return Err(RunAbort::RiskControl(m)),
            Err(e) => {
                ctx.warn(format!("首次认领失败（{e}），按全部需要上传处理"));
            }
        }
    }

    // 5. 串行上传
    let mut dirs = RemoteDirCache::load(store, job_id).map_err(RunAbort::Error)?;
    ensure_dir_cached(ctx, dest, store, &mut dirs, remote_dir).await?;

    let mut streak = 0u32;
    for f in &candidates {
        ctx.checkpoint()?;
        let dst = remote_path_of(remote_dir, &f.rel);
        let (dir, name) = split_path(&dst);
        if !dirs.contains(&dir) {
            match dest.ensure_dir(&dir).await {
                Ok(()) => dirs.insert(store, &dir).map_err(RunAbort::Error)?,
                Err(DestError::RiskControl(m)) => return Err(RunAbort::RiskControl(m)),
                Err(e) => {
                    streak = register_failure(ctx, &f.rel, &e, streak)?;
                    continue;
                }
            }
        }

        let temp = join_path(&dir, &format!("{name}.bmpart"));
        let pending_id = store.add_pending(job_id, &temp).map_err(RunAbort::Error)?;
        let expect_existing = index.contains_key(&f.rel) || replaced.contains(&f.rel);

        match upload_one(ctx, dest, f, &dst, expect_existing).await {
            Ok(out) => {
                store.remove_pending(pending_id).ok();
                store
                    .upsert_file(&index_row(
                        job_id,
                        &f.rel,
                        out.size,
                        f.mtime_ns,
                        &dst,
                    ))
                    .map_err(RunAbort::Error)?;
                ctx.with_stats(|s| {
                    s.uploaded += 1;
                    s.uploaded_bytes += out.size.max(0) as u64;
                });
                ctx.info(format!("已上传 {}（{}）", f.rel, human(out.size.max(0) as u64)));
                if opts.delete_local {
                    remove_local(ctx, opts, Path::new(&opts.source), f);
                }
                streak = 0;
            }
            Err(e) => {
                streak = register_failure(ctx, &f.rel, &e, streak)?;
            }
        }

        tokio::time::sleep(Duration::from_millis(policy.file_gap_ms)).await;
    }

    // 6. 删本地：本次跳过但云端已确认过的文件
    if opts.delete_local {
        if !already_in_cloud.is_empty() {
            ctx.info(format!(
                "清理本地：{} 个文件云端已有确认记录（未变化），按设置删除本地副本",
                already_in_cloud.len()
            ));
            for f in &already_in_cloud {
                ctx.checkpoint()?;
                remove_local(ctx, opts, Path::new(&opts.source), f);
            }
        }
        prune_empty_dirs(ctx, Path::new(&opts.source));
    }

    // 7. 同步删除
    if opts.sync_delete {
        let local: HashSet<&str> = scan.files.iter().map(|f| f.rel.as_str()).collect();
        let mut gone: Vec<FileRow> = index
            .values()
            .filter(|r| !local.contains(r.rel.as_str()))
            .cloned()
            .collect();
        gone.sort_by(|a, b| a.rel.cmp(&b.rel));
        if gone.is_empty() {
            ctx.info("同步删除：没有需要在云端删除的文件");
        } else {
            ctx.info(format!("同步删除：{} 个文件在源端已不存在", gone.len()));
            for row in gone {
                ctx.checkpoint()?;
                match dest.delete(&row.remote_path).await {
                    Ok(()) | Err(DestError::NotFound(_)) => {
                        store.delete_file_row(job_id, &row.rel).map_err(RunAbort::Error)?;
                        ctx.with_stats(|s| s.deleted += 1);
                        ctx.info(format!("已从云端删除 {}", row.remote_path));
                    }
                    Err(DestError::RiskControl(m)) => return Err(RunAbort::RiskControl(m)),
                    Err(e) => {
                        streak = register_failure(ctx, &row.rel, &e, streak)?;
                    }
                }
                tokio::time::sleep(Duration::from_millis(policy.delete_gap_ms)).await;
            }
        }
    }

    Ok(())
}


/// 预览：只扫本地并与索引比对，不碰远端、不写任何东西。
#[derive(serde::Serialize)]
pub struct Preview {
    pub scanned: u64,
    pub candidates: u64,
    pub unchanged: u64,
    pub excluded: u64,
    pub symlinks: u64,
    pub first_run: bool,
    pub sample: Vec<String>,
}

pub async fn preview(
    store: &Store,
    job_id: i64,
    remote_dir: &str,
    opts: &MirrorOptions,
) -> Result<Preview, String> {
    if opts.source.trim().is_empty() {
        return Err("源目录不能为空".to_string());
    }
    let include = compile(&opts.include)?;
    let exclude = compile(&opts.exclude)?;
    let root = Path::new(&opts.source).to_path_buf();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let scan = tokio::task::spawn_blocking(move || scan_local(&root, &include, &exclude, &cancel))
        .await
        .map_err(|e| e.to_string())??;
    let index: HashMap<String, FileRow> = store
        .load_files(job_id)?
        .into_iter()
        .map(|r| (r.rel.clone(), r))
        .collect();
    let mut candidates = 0u64;
    let mut sample = Vec::new();
    for f in &scan.files {
        let expected = remote_path_of(remote_dir, &f.rel);
        let changed = match index.get(&f.rel) {
            Some(r) => !(r.size == f.size && r.mtime_ns == f.mtime_ns && r.remote_path == expected),
            None => true,
        };
        if changed {
            candidates += 1;
            if sample.len() < 20 {
                sample.push(f.rel.clone());
            }
        }
    }
    Ok(Preview {
        scanned: scan.files.len() as u64,
        candidates,
        unchanged: scan.files.len() as u64 - candidates,
        excluded: scan.skipped_excluded,
        symlinks: scan.skipped_symlinks,
        first_run: index.is_empty(),
        sample,
    })
}

/// 删除本地文件前复核：扫描之后这个文件没有被写端替换过。
///
/// “上传后删除本地”常配合投递目录（SMB 中转）：客户端可能在“扫描 → 上传 → 删本地”
/// 之间把新版本 rename 到位。这时删掉会把新版本丢掉，而索引记录的还是旧版本，
/// 客户端清单又认为已发送，云端就停在旧内容上。所以删除前必须再 stat 一次。
fn local_still_matches(f: &LocalFile) -> bool {
    match std::fs::symlink_metadata(&f.abs) {
        Ok(meta) => {
            meta.is_file() && meta.len() as i64 == f.size && super::mtime_ns(&meta) == f.mtime_ns
        }
        Err(_) => false,
    }
}

/// 删除（或移到回收目录）一个已确认在云端的本地文件。
fn remove_local(ctx: &RunCtx, opts: &MirrorOptions, root: &Path, f: &LocalFile) {
    if !local_still_matches(f) {
        ctx.warn(format!(
            "{}: 扫描后已被修改或已不存在，保留本地文件，下一轮再处理",
            f.rel
        ));
        return;
    }
    let path = Path::new(&f.abs);
    let root_real = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let file_real = match path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            ctx.warn(format!("{}: 取真实路径失败，跳过删除（{e}）", f.rel));
            return;
        }
    };
    if !file_real.starts_with(&root_real) || file_real == root_real {
        ctx.warn(format!("{}: 不在源目录内，跳过删除", f.rel));
        return;
    }

    let trash = opts
        .local_trash
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match trash {
        Some(trash_root) => {
            let target = Path::new(trash_root).join(&f.rel);
            if let Some(dir) = target.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::rename(&file_real, &target) {
                Ok(()) => {
                    ctx.with_stats(|s| s.local_deleted += 1);
                    ctx.info(format!("已移到回收目录 {} → {}", f.rel, target.display()));
                }
                Err(e) => ctx.warn(format!(
                    "{}: 移到回收目录失败（可能不在同一文件系统），保留本地文件：{e}",
                    f.rel
                )),
            }
        }
        None => match std::fs::remove_file(&file_real) {
            Ok(()) => {
                ctx.with_stats(|s| s.local_deleted += 1);
                ctx.info(format!("已删除本地文件 {}", f.rel));
            }
            Err(e) => ctx.warn(format!("{}: 删除本地文件失败：{e}", f.rel)),
        },
    }
}

/// 清理源目录里变空的子目录（不动源目录本身）。
fn prune_empty_dirs(ctx: &RunCtx, root: &Path) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(entry.path());
            }
        }
        if dir != root {
            dirs.push(dir);
        }
    }
    // 深度优先：先删最深的
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    let mut removed = 0usize;
    for dir in dirs {
        let empty = std::fs::read_dir(&dir)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false);
        if empty && std::fs::remove_dir(&dir).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        ctx.info(format!("清理了 {removed} 个空目录"));
    }
}

fn compile(patterns: &[String]) -> Result<GlobSet, String> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let glob = Glob::new(p).map_err(|e| format!("规则 {p} 不合法: {e}"))?;
        b.add(glob);
    }
    b.build().map_err(|e| format!("规则集构建失败: {e}"))
}

/// 一次性读取远端备份目录，返回 rel -> 远端项。
async fn adopt(
    ctx: &RunCtx,
    dest: &dyn Destination,
    remote_dir: &str,
) -> Result<HashMap<String, RemoteEntry>, DestError> {
    let mut out = HashMap::new();
    if dest.stat(remote_dir).await?.is_none() {
        ctx.info(format!("云端备份目录 {remote_dir} 还不存在，无需认领"));
        return Ok(out);
    }
    let mut stack = vec![(remote_dir.to_string(), String::new())];
    let mut dirs = 0u32;
    while let Some((dir, prefix)) = stack.pop() {
        if ctx.is_cancelled() {
            break;
        }
        let entries = match dest.list_dir(&dir).await {
            Ok(e) => e,
            Err(DestError::NotFound(_)) => continue,
            Err(e) => return Err(e),
        };
        dirs += 1;
        for e in entries {
            let rel = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            if e.is_dir {
                stack.push((join_path(&dir, &e.name), rel));
            } else {
                out.insert(rel, e);
            }
        }
        if dirs.is_multiple_of(20) {
            ctx.info(format!("已读取 {dirs} 个远端目录（限速中）"));
        }
        if dirs > 5000 {
            ctx.warn("远端目录超过 5000 个，停止继续认领，其余按需要上传处理");
            break;
        }
        tokio::time::sleep(Duration::from_millis(ctx.policy().list_gap_ms)).await;
    }
    ctx.info(format!("远端读取完成：{dirs} 个目录、{} 个文件", out.len()));
    Ok(out)
}

async fn upload_one(
    ctx: &RunCtx,
    dest: &dyn Destination,
    f: &LocalFile,
    dst: &str,
    expect_existing: bool,
) -> Result<crate::destination::UploadOutcome, DestError> {
    let total = f.size.max(0) as u64;
    ctx.info(format!(
        "开始上传 {}（{}，{}）",
        f.rel,
        human(total),
        if expect_existing { "覆盖旧版本" } else { "新增" }
    ));
    let file = tokio::fs::File::open(&f.abs)
        .await
        .map_err(|e| DestError::Other(format!("打开 {} 失败: {e}", f.abs)))?;
    let progress = Arc::new(AtomicU64::new(0));
    let cancel = ctx.cancel_flag();

    let ticker = {
        let ctx = ctx.clone();
        let progress = progress.clone();
        let rel = f.rel.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                if ctx.is_cancelled() {
                    return;
                }
                let sent = progress.load(Ordering::Relaxed);
                ctx.info(format!("{rel}: 已发送 {} / {}", human(sent), human(total)));
            }
        })
    };

    let result = dest
        .upload(UploadRequest {
            dst_path: dst,
            reader: Box::new(file),
            expected_len: Some(total),
            expect_existing,
            progress,
            cancel,
        })
        .await;
    ticker.abort();
    result
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::fake::{FakeDest, fast_policy, test_ctx};
    use crate::model::MirrorOptions;
    use crate::store::Store;

    fn sample_source() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"bbb").unwrap();
        std::fs::create_dir_all(dir.path().join("@eaDir")).unwrap();
        std::fs::write(dir.path().join("@eaDir/thumb.jpg"), b"x").unwrap();
        dir
    }

    fn opts(source: &str) -> MirrorOptions {
        MirrorOptions {
            source: source.to_string(),
            include: vec![],
            exclude: vec!["@eaDir".to_string()],
            sync_delete: false,
            skip_adopt: false,
            delete_local: false,
            local_trash: None,
        }
    }

    async fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn 首次全部上传_二次不再重传也不读远端() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        let o = opts(src.path().to_str().unwrap());

        let (ctx, _log) = test_ctx(fast_policy());
        run(&ctx, &dest, &store, 1, "/115open/backup", &o).await.unwrap();
        assert_eq!(dest.upload_count(), 2, "两个文件（@eaDir 被排除）");
        assert!(dest.exists("/115open/backup/a.txt"));
        assert!(dest.exists("/115open/backup/sub/b.txt"));
        assert!(!dest.exists("/115open/backup/@eaDir/thumb.jpg"));
        let lists = dest.list_count();

        let (ctx2, _log2) = test_ctx(fast_policy());
        run(&ctx2, &dest, &store, 1, "/115open/backup", &o).await.unwrap();
        assert_eq!(dest.upload_count(), 2, "未变化不应重传");
        assert_eq!(dest.list_count(), lists, "二次运行不应再列远端目录");
        assert_eq!(ctx2.stats_snapshot().skipped, 2);
    }

    #[tokio::test]
    async fn 首次运行认领内容一致的远端文件() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        dest.put("/115open/backup/a.txt", b"aaa");
        let o = opts(src.path().to_str().unwrap());

        let (ctx, _log) = test_ctx(fast_policy());
        run(&ctx, &dest, &store, 1, "/115open/backup", &o).await.unwrap();
        let stats = ctx.stats_snapshot();
        assert_eq!(stats.adopted, 1);
        assert_eq!(dest.upload_count(), 1, "只传没认领的那个");
    }

    #[tokio::test]
    async fn 文件改过会重新上传() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        let o = opts(src.path().to_str().unwrap());

        let (ctx, _log) = test_ctx(fast_policy());
        run(&ctx, &dest, &store, 1, "/115open/backup", &o).await.unwrap();
        assert_eq!(dest.upload_count(), 2);

        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(src.path().join("a.txt"), b"aaa-changed").unwrap();
        let (ctx2, _log2) = test_ctx(fast_policy());
        run(&ctx2, &dest, &store, 1, "/115open/backup", &o).await.unwrap();
        assert_eq!(dest.upload_count(), 3);
        assert_eq!(dest.content("/115open/backup/a.txt"), Some(b"aaa-changed".to_vec()));
    }

    #[tokio::test]
    async fn 同步删除只删索引里的文件() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        let mut o = opts(src.path().to_str().unwrap());
        o.sync_delete = true;

        let (ctx, _log) = test_ctx(fast_policy());
        run(&ctx, &dest, &store, 1, "/115open/backup", &o).await.unwrap();
        assert!(dest.exists("/115open/backup/sub/b.txt"));

        std::fs::remove_file(src.path().join("sub/b.txt")).unwrap();
        let (ctx2, _log2) = test_ctx(fast_policy());
        run(&ctx2, &dest, &store, 1, "/115open/backup", &o).await.unwrap();
        assert!(!dest.exists("/115open/backup/sub/b.txt"), "源端已删，云端也应删");
        assert!(dest.exists("/115open/backup/a.txt"), "其它文件不能动");
    }

    #[tokio::test]
    async fn 源目录为空时不执行同步删除() {
        let src = tempfile::tempdir().unwrap();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        dest.put("/115open/backup/keep.txt", b"keep");
        let mut o = opts(src.path().to_str().unwrap());
        o.sync_delete = true;
        // 索引里先记一条，模拟之前备份过
        store
            .upsert_file(&index_row(1, "keep.txt", 4, 0, "/115open/backup/keep.txt"))
            .unwrap();

        let (ctx, _log) = test_ctx(fast_policy());
        run(&ctx, &dest, &store, 1, "/115open/backup", &o).await.unwrap();
        assert!(dest.exists("/115open/backup/keep.txt"), "空源目录不得清空云端");
    }

    #[tokio::test]
    async fn 连续失败达到阈值就中止() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        *dest.fail_uploads.lock().unwrap() = 10;
        let o = opts(src.path().to_str().unwrap());

        let (ctx, _log) = test_ctx(fast_policy()); // max_failures = 2
        let result = run(&ctx, &dest, &store, 1, "/115open/backup", &o).await;
        assert!(matches!(result, Err(RunAbort::TooManyFailures(2))), "实际: {result:?}");
        assert_eq!(ctx.stats_snapshot().failed, 2);
    }

    #[tokio::test]
    async fn 风控错误立刻中止整轮() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        *dest.risk_on.lock().unwrap() = Some("a.txt".to_string());
        let o = opts(src.path().to_str().unwrap());

        let (ctx, _log) = test_ctx(fast_policy());
        let result = run(&ctx, &dest, &store, 1, "/115open/backup", &o).await;
        assert!(matches!(result, Err(RunAbort::RiskControl(_))), "实际: {result:?}");
    }

    #[tokio::test]
    async fn 中断留下的临时文件会被清理() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        dest.put("/115open/backup/stale.bin.bmpart", b"partial");
        store.add_pending(1, "/115open/backup/stale.bin.bmpart").unwrap();
        let o = opts(src.path().to_str().unwrap());

        let (ctx, _log) = test_ctx(fast_policy());
        run(&ctx, &dest, &store, 1, "/115open/backup", &o).await.unwrap();
        assert!(!dest.exists("/115open/backup/stale.bin.bmpart"));
        assert!(store.list_pending(1).unwrap().is_empty());
    }

    #[tokio::test]
    async fn 上传校验通过后删除本地文件() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        let mut o = opts(src.path().to_str().unwrap());
        o.delete_local = true;

        let (ctx, _log) = test_ctx(fast_policy());
        run(&ctx, &dest, &store, 1, "/115open/backup", &o).await.unwrap();

        assert_eq!(dest.upload_count(), 2);
        assert_eq!(ctx.stats_snapshot().local_deleted, 2);
        assert!(!src.path().join("a.txt").exists(), "本地上传后应删除");
        assert!(!src.path().join("sub/b.txt").exists());
        assert!(!src.path().join("sub").exists(), "空目录也应清理");
        // @eaDir 被排除，本地文件不该动
        assert!(src.path().join("@eaDir/thumb.jpg").exists());
        // 云端有文件
        assert!(dest.exists("/115open/backup/a.txt"));
    }

    #[tokio::test]
    async fn 上传失败时不动本地文件() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        *dest.fail_uploads.lock().unwrap() = 10;
        let mut o = opts(src.path().to_str().unwrap());
        o.delete_local = true;

        let (ctx, _log) = test_ctx(fast_policy());
        let _ = run(&ctx, &dest, &store, 1, "/115open/backup", &o).await;
        assert!(src.path().join("a.txt").exists(), "上传失败不能删本地");
        assert!(src.path().join("sub/b.txt").exists());
        assert_eq!(ctx.stats_snapshot().local_deleted, 0);
    }

    #[tokio::test]
    async fn 删本地可以改为移到回收目录() {
        let src = sample_source();
        let trash = tempfile::tempdir().unwrap();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        let mut o = opts(src.path().to_str().unwrap());
        o.delete_local = true;
        o.local_trash = Some(trash.path().to_string_lossy().to_string());

        let (ctx, _log) = test_ctx(fast_policy());
        run(&ctx, &dest, &store, 1, "/115open/backup", &o).await.unwrap();
        assert!(!src.path().join("a.txt").exists());
        assert!(trash.path().join("a.txt").exists(), "应移到回收目录");
        assert!(trash.path().join("sub/b.txt").exists());
    }

    #[tokio::test]
    async fn 与同步删除互斥() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        let mut o = opts(src.path().to_str().unwrap());
        o.delete_local = true;
        o.sync_delete = true;
        let (ctx, _log) = test_ctx(fast_policy());
        let result = run(&ctx, &dest, &store, 1, "/115open/backup", &o).await;
        assert!(matches!(result, Err(RunAbort::Error(_))), "实际: {result:?}");
        assert!(src.path().join("a.txt").exists(), "校验失败不能删本地");
    }

    #[tokio::test]
    async fn 已确认在云端的旧文件也会被清理() {
        let src = sample_source();
        let (_sd, store) = test_store().await;
        let dest = FakeDest::new();
        // 第一次不带删本地，建立索引
        let o1 = opts(src.path().to_str().unwrap());
        let (ctx1, _l1) = test_ctx(fast_policy());
        run(&ctx1, &dest, &store, 1, "/115open/backup", &o1).await.unwrap();
        assert!(src.path().join("a.txt").exists());

        // 第二次打开删本地：文件未变化，但索引里有记录 → 直接清本地
        let mut o2 = opts(src.path().to_str().unwrap());
        o2.delete_local = true;
        let (ctx2, _l2) = test_ctx(fast_policy());
        run(&ctx2, &dest, &store, 1, "/115open/backup", &o2).await.unwrap();
        assert_eq!(dest.upload_count(), 2, "未变化不应重传");
        assert!(!src.path().join("a.txt").exists());
        assert_eq!(ctx2.stats_snapshot().local_deleted, 2);
    }

    #[test]
    fn 体积格式化() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn 非法规则要报错() {
        assert!(compile(&["[".to_string()]).is_err());
        assert!(compile(&["*.mkv".to_string()]).is_ok());
    }

    #[test]
    fn 删除前复核文件是否仍与扫描一致() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"aaa").unwrap();

        let make = |p: &Path| {
            let meta = std::fs::metadata(p).unwrap();
            LocalFile {
                rel: "a.txt".to_string(),
                size: meta.len() as i64,
                mtime_ns: crate::jobs::mtime_ns(&meta),
                abs: p.to_string_lossy().to_string(),
            }
        };

        let f = make(&path);
        assert!(local_still_matches(&f), "刚写入的文件应一致");

        // 大小变了：删掉会把客户端刚换上的新版本弄丢
        std::fs::write(&path, b"aaaa").unwrap();
        assert!(!local_still_matches(&f));

        // 大小相同但 mtime 变了
        std::fs::write(&path, b"bbb").unwrap();
        let f2 = make(&path);
        assert!(local_still_matches(&f2));
        let times = std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000));
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(times)
            .unwrap();
        assert!(!local_still_matches(&f2), "mtime 变了不能删");

        // 文件已经不存在
        std::fs::remove_file(&path).unwrap();
        assert!(!local_still_matches(&f2));
    }
}
