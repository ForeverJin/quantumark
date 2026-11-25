// 目录归档：每次运行把源目录打成一个 tar.zst（可选 age 加密），流式写给 CD2。
//
// 不落临时文件：tar → zstd →（age）→ 直接作为上传的数据源。
// 只有上传成功且打包进程全部正常退出，才记录这次归档并做保留份数轮转。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::pipeline::{Pipeline, StageSpec, program_available};
use super::{
    RemoteDirCache, RunAbort, RunCtx, apply_retention, cleanup_pending, ensure_dir_cached,
};
use crate::destination::{Destination, UploadRequest, join_path};
use crate::model::ArchiveOptions;
use crate::store::Store;

pub async fn run(
    ctx: &RunCtx,
    dest: &dyn Destination,
    store: &Store,
    job_id: i64,
    remote_dir: &str,
    opts: &ArchiveOptions,
    keep: Option<i64>,
) -> Result<(), RunAbort> {
    let policy = ctx.policy().clone();
    let src = Path::new(&opts.source).to_path_buf();
    if !src.is_dir() {
        return Err(RunAbort::Error(format!("源目录不存在或不是目录: {}", opts.source)));
    }
    let parent = src
        .parent()
        .ok_or_else(|| RunAbort::Error("源目录没有上级目录，无法打包".to_string()))?
        .to_path_buf();
    let dir_name = src
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| RunAbort::Error("源目录名不是合法 UTF-8".to_string()))?
        .to_string();

    let recipient = opts
        .age_recipient
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

    if !program_available("tar").await {
        return Err(RunAbort::Error("容器里找不到 tar".to_string()));
    }
    if !program_available("zstd").await {
        return Err(RunAbort::Error("容器里找不到 zstd".to_string()));
    }
    if recipient.is_some() && !program_available("age").await {
        return Err(RunAbort::Error("选择了加密，但容器里找不到 age".to_string()));
    }

    cleanup_pending(ctx, dest, store, job_id).await?;
    let mut dirs = RemoteDirCache::load(store, job_id).map_err(RunAbort::Error)?;
    ensure_dir_cached(ctx, dest, store, &mut dirs, remote_dir).await?;

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let ext = if recipient.is_some() { ".tar.zst.age" } else { ".tar.zst" };
    let file_name = format!("{ts}{ext}");
    let dst = join_path(remote_dir, &file_name);
    let temp = join_path(remote_dir, &format!("{file_name}.bmpart"));
    let pending_id = store.add_pending(job_id, &temp).map_err(RunAbort::Error)?;

    // tar 的路径参数：-C 上级目录，然后只写目录名，包内就是干净的顶层目录
    let mut tar_args = vec![
        "-cf".to_string(),
        "-".to_string(),
        "-C".to_string(),
        parent.display().to_string(),
    ];
    for e in &opts.exclude {
        tar_args.push(format!("--exclude={e}"));
    }
    tar_args.push(dir_name.clone());

    let mut stages = vec![StageSpec::new("tar", tar_args)];
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

    ctx.with_stats(|s| {
        s.scanned = 1;
        s.candidates = 1;
    });
    ctx.info(format!(
        "开始打包 {} → {}（{}）",
        opts.source,
        dst,
        if recipient.is_some() { "zstd 压缩 + age 加密" } else { "zstd 压缩" }
    ));
    let mut pipe = Pipeline::start(stages).map_err(RunAbort::Error)?;
    let reader = pipe.take_reader();

    let progress = Arc::new(AtomicU64::new(0));
    let ticker = {
        let ctx = ctx.clone();
        let progress = progress.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                if ctx.is_cancelled() {
                    return;
                }
                let sent = progress.load(Ordering::Relaxed);
                if sent > 0 {
                    ctx.info(format!("已发送 {}", super::mirror::human(sent)));
                }
            }
        })
    };

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

    match upload {
        Ok(out) => match pipe.finish().await {
            Ok(()) => {
                store.remove_pending(pending_id).ok();
                store
                    .add_archive(job_id, &dst, out.size, out.sha1.as_deref())
                    .map_err(RunAbort::Error)?;
                ctx.with_stats(|s| {
                    s.uploaded += 1;
                    s.uploaded_bytes += out.size.max(0) as u64;
                });
                ctx.info(format!(
                    "归档完成：{}（{}）",
                    dst,
                    super::mirror::human(out.size.max(0) as u64)
                ));
            }
            Err(e) => {
                // 打包过程中有错误（比如某个文件读不了），这个包不可信：
                // 删掉刚传上去的文件，也不记录归档。
                ctx.error(format!("打包进程异常：{e}"));
                let _ = dest.delete(&dst).await;
                store.remove_pending(pending_id).ok();
                return Err(RunAbort::Error(format!("打包失败，已删除不完整归档: {e}")));
            }
        },
        Err(e) => {
            pipe.kill();
            ctx.error(format!("上传失败：{e}"));
            return Err(match e {
                crate::destination::DestError::RiskControl(m) => RunAbort::RiskControl(m),
                other => RunAbort::Error(format!("上传失败: {other}")),
            });
        }
    }

    if let Some(keep) = keep {
        apply_retention(ctx, dest, store, job_id, remote_dir, keep).await?;
    }
    tokio::time::sleep(Duration::from_millis(policy.file_gap_ms)).await;
    Ok(())
}
