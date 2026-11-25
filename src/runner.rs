// 任务运行器：全局串行执行，负责运行记录、日志、取消和风控冷却。
//
// 单实例部署，所以用一个工作协程 + 队列就够；同一任务不会并发跑。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::{Mutex, mpsc};

use crate::cd2::Cd2Destination;
use crate::config::Config;
use crate::jobs::{RunAbort, RunCtx, archive, mirror, mysql};
use crate::model::{ArchiveOptions, DestSecret, JobKind, JobSecret, MirrorOptions, MysqlOptions};
use crate::secrets::Sealer;
use crate::store::{Store, now};

/// 风控冷却的存储键。
const COOLDOWN_KEY: &str = "cooldown_until";

#[derive(Clone)]
pub struct Runner {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<Store>,
    sealer: Arc<Sealer>,
    config: Config,
    tx: mpsc::Sender<(i64, String)>,
    /// 正在跑或排队中的任务
    active: Mutex<HashSet<i64>>,
    /// 正在跑的任务 → 取消标志
    cancels: Mutex<HashMap<i64, Arc<AtomicBool>>>,
}

impl Runner {
    pub fn start(store: Arc<Store>, sealer: Arc<Sealer>, config: Config) -> Runner {
        let (tx, mut rx) = mpsc::channel::<(i64, String)>(64);
        let inner = Arc::new(Inner {
            store,
            sealer,
            config,
            tx,
            active: Mutex::new(HashSet::new()),
            cancels: Mutex::new(HashMap::new()),
        });
        let runner = Runner { inner: inner.clone() };
        tokio::spawn(async move {
            while let Some((job_id, trigger)) = rx.recv().await {
                let worker = Runner { inner: inner.clone() };
                if let Err(e) = worker.run_job(job_id, &trigger).await {
                    tracing::error!(job_id, "任务执行失败: {e}");
                }
                inner.active.lock().await.remove(&job_id);
            }
        });
        runner
    }

    /// 排入队列。已在跑或排队时返回错误。
    pub async fn enqueue(&self, job_id: i64, trigger: &str) -> Result<(), String> {
        {
            let mut active = self.inner.active.lock().await;
            if active.contains(&job_id) {
                return Err("任务正在运行或排队中".to_string());
            }
            active.insert(job_id);
        }
        if let Err(e) = self.inner.tx.send((job_id, trigger.to_string())).await {
            self.inner.active.lock().await.remove(&job_id);
            return Err(format!("任务队列不可用: {e}"));
        }
        tracing::info!(job_id, trigger, "任务已排队");
        Ok(())
    }

    /// 请求停止正在跑的任务。
    pub async fn stop(&self, job_id: i64) -> Result<(), String> {
        let cancels = self.inner.cancels.lock().await;
        match cancels.get(&job_id) {
            Some(flag) => {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            None => Err("任务没有在运行".to_string()),
        }
    }

    pub async fn is_active(&self, job_id: i64) -> bool {
        self.inner.active.lock().await.contains(&job_id)
    }

    async fn run_job(&self, job_id: i64, trigger: &str) -> Result<(), String> {
        let store = self.inner.store.clone();
        let job = store.get_job(job_id)?.ok_or_else(|| "任务不存在".to_string())?;
        let policy = store.policy();

        // 定时触发时遵守冷却；手动触发允许，但会先警告。
        if let Some(until) = get_cooldown(&store) {
            if until > now() {
                let until_text = chrono::DateTime::from_timestamp(until, 0)
                    .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
                    .unwrap_or_else(|| "稍后".to_string());
                if trigger == "schedule" {
                    tracing::warn!(job_id, "处于风控冷却期（到 {until_text}），跳过本轮");
                    return Ok(());
                }
            }
        }

        // 目的地
        let dest_row = store
            .get_destination(job.destination_id)?
            .ok_or_else(|| "任务用的目的地已被删除".to_string())?;
        let dest_cfg = dest_row.config()?;
        let secret = decrypt_dest_secret(&self.inner.sealer, &dest_row)?;

        // 日志文件
        let log_dir = self.inner.config.log_dir();
        std::fs::create_dir_all(&log_dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let log_path = log_dir.join(format!("job{job_id}-{stamp}.log"));
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("创建运行日志失败: {e}"))?;

        let run_id = store.start_run(job_id, trigger, &log_path.display().to_string())?;
        let cancel = Arc::new(AtomicBool::new(false));
        self.inner.cancels.lock().await.insert(job_id, cancel.clone());
        let ctx = RunCtx::new(run_id, &job.name, policy.clone(), cancel, log_file);

        ctx.info(format!(
            "开始运行：{}（{}，目的地 {} {}，远端目录 {}）",
            job.name,
            job.kind,
            dest_row.name,
            dest_cfg.address,
            job.remote_dir
        ));

        let destination = match Cd2Destination::new(&dest_cfg.address, secret, &dest_cfg.root, &policy)
        {
            Ok(d) => d,
            Err(e) => {
                finish(&store, &ctx, run_id, Err(RunAbort::Error(format!("目的地不可用: {e}"))));
                self.inner.cancels.lock().await.remove(&job_id);
                return Ok(());
            }
        };

        let kind = match job.kind() {
            Ok(k) => k,
            Err(e) => {
                finish(&store, &ctx, run_id, Err(RunAbort::Error(e)));
                self.inner.cancels.lock().await.remove(&job_id);
                return Ok(());
            }
        };

        let result = match kind {
            JobKind::Mirror => match serde_json::from_str::<MirrorOptions>(&job.options_json) {
                Ok(opts) => {
                    mirror::run(&ctx, &destination, &store, job_id, &job.remote_dir, &opts).await
                }
                Err(e) => Err(RunAbort::Error(format!("任务参数损坏: {e}"))),
            },
            JobKind::Archive => match serde_json::from_str::<ArchiveOptions>(&job.options_json) {
                Ok(opts) => {
                    archive::run(
                        &ctx,
                        &destination,
                        &store,
                        job_id,
                        &job.remote_dir,
                        &opts,
                        job.keep,
                    )
                    .await
                }
                Err(e) => Err(RunAbort::Error(format!("任务参数损坏: {e}"))),
            },
            JobKind::Mysql => {
                let opts = serde_json::from_str::<MysqlOptions>(&job.options_json);
                let secret = decrypt_job_secret(&self.inner.sealer, job.secret_sealed.as_deref());
                match (opts, secret) {
                    (Ok(opts), Ok(secret)) => {
                        mysql::run(
                            &ctx,
                            &destination,
                            &store,
                            job_id,
                            &job.remote_dir,
                            &opts,
                            &secret,
                            job.keep,
                        )
                        .await
                    }
                    (Err(e), _) => Err(RunAbort::Error(format!("任务参数损坏: {e}"))),
                    (_, Err(e)) => Err(RunAbort::Error(e)),
                }
            }
        };

        // 风控：整轮停止并写冷却
        if let Err(RunAbort::RiskControl(msg)) = &result {
            let until = now() + (policy.cooldown_minutes as i64) * 60;
            set_cooldown(&store, until);
            ctx.warn(format!(
                "触发风控/限流：{msg}；已设置 {} 分钟冷却，期间定时任务不会启动",
                policy.cooldown_minutes
            ));
        }

        finish(&store, &ctx, run_id, result);
        self.inner.cancels.lock().await.remove(&job_id);
        Ok(())
    }
}

fn finish(
    store: &Store,
    ctx: &RunCtx,
    run_id: i64,
    result: Result<(), RunAbort>,
) {
    let stats = ctx.stats_snapshot();
    let (status, error) = match &result {
        Ok(()) => {
            if stats.failed > 0 {
                (
                    "partial",
                    Some(format!("{} 个文件失败，其余已完成", stats.failed)),
                )
            } else {
                ("success", None)
            }
        }
        Err(abort) => {
            let (status, message) = abort.status_message();
            (status, Some(message))
        }
    };
    ctx.info(format!(
        "统计：扫描 {}，需处理 {}，上传 {}（{}），认领 {}，跳过 {}，删除 {}，失败 {}",
        stats.scanned,
        stats.candidates,
        stats.uploaded,
        crate::jobs::mirror::human(stats.uploaded_bytes),
        stats.adopted,
        stats.skipped,
        stats.deleted,
        stats.failed
    ));
    ctx.info(match status {
        "success" => "本轮结束：成功".to_string(),
        "cancelled" => "本轮结束：已停止".to_string(),
        other => format!(
            "本轮结束：{}（{}）",
            other,
            error.clone().unwrap_or_default()
        ),
    });
    if let Err(e) = store.finish_run(run_id, status, &stats, error.as_deref()) {
        tracing::error!(run_id, "写入运行记录失败: {e}");
    }
}

fn decrypt_job_secret(sealer: &Sealer, sealed: Option<&str>) -> Result<JobSecret, String> {
    match sealed {
        None => Ok(JobSecret::default()),
        Some(s) => {
            let plain = sealer.open(s)?;
            serde_json::from_str(&plain).map_err(|e| format!("任务凭据损坏: {e}"))
        }
    }
}

fn decrypt_dest_secret(sealer: &Sealer, row: &crate::model::DestinationRow) -> Result<DestSecret, String> {
    let sealed = row
        .secret_sealed
        .as_deref()
        .ok_or_else(|| format!("目的地 {} 还没有配置凭据", row.name))?;
    let plain = sealer.open(sealed)?;
    serde_json::from_str(&plain).map_err(|e| format!("目的地凭据损坏: {e}"))
}

/// 当前冷却截止时间（unix 秒）。
pub fn get_cooldown(store: &Store) -> Option<i64> {
    store
        .get_setting(COOLDOWN_KEY)
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
}

pub fn set_cooldown(store: &Store, until: i64) {
    if let Err(e) = store.set_setting(COOLDOWN_KEY, &until.to_string()) {
        tracing::error!("写入冷却时间失败: {e}");
    }
}
