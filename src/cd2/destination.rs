// CD2 目的地实现：把通用的 Destination 操作翻译成 CD2 的 gRPC 调用。
//
// 上传流程（关键设计）：
//   1. 先在云端建 <名字>.bmpart 临时文件并写入数据，由 CD2 负责上传到 115；
//   2. 等 CD2 的上传任务到终态，核对大小；
//   3. 校验通过后才改成正式名（旧版本在此之前一直保留）。
//   这样既不依赖 CD2 对同名文件的处理策略，也不会出现「新版没传完、旧版已删除」。

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::client::{Cd2Client, Cd2Error, UploadState, is_risk_control, to_remote_entry};
use crate::destination::{
    DestError, Destination, PermissionSummary, ProbeReport, RemoteEntry, SpaceView, UploadOutcome,
    UploadRequest, join_path, split_path,
};
use crate::model::{DestSecret, Policy};

/// 阿里/Debian 上的进度判定的下限速度：按 128 KiB/s 估算等待上传完成的总时长。
const MIN_SPEED_BYTES_PER_SEC: u64 = 128 * 1024;

pub struct Cd2Destination {
    client: Cd2Client,
    root: String,
    policy: Policy,
    /// API Token 原文（用于查令牌权限），Password 模式为 None
    token: Option<String>,
}

impl Cd2Destination {
    pub fn new(
        address: &str,
        secret: DestSecret,
        root: &str,
        policy: &Policy,
    ) -> Result<Cd2Destination, DestError> {
        let root = root.trim_end_matches('/').to_string();
        if root.is_empty() || !root.starts_with('/') {
            return Err(DestError::Other(format!("根路径要以 / 开头，例如 /115open（当前: {root}）")));
        }
        let token = match &secret {
            DestSecret::Token { token } => Some(token.clone()),
            DestSecret::Password { .. } => None,
        };
        let client = Cd2Client::new(address, secret, policy).map_err(|e| e.into_dest())?;
        Ok(Cd2Destination { client, root, policy: policy.clone(), token })
    }

    /// 等待 CD2 把这个临时文件真正传完。
    async fn wait_upload_done(
        &self,
        temp_path: &str,
        expected_len: u64,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(UploadState, Option<u64>), DestError> {
        let wait_budget = Duration::from_secs(600)
            + Duration::from_secs(expected_len / MIN_SPEED_BYTES_PER_SEC);
        let deadline = Instant::now() + wait_budget.min(Duration::from_secs(24 * 3600));
        let poll_max = Duration::from_millis(self.policy.poll_ms.max(1000));
        // 自适应轮询：刚传完时密集查几次（秒传可能立刻完成），没完成再逐步放慢。
        // 查的是 CD2 本地的任务列表，不走 115，代价很小。
        let mut interval = Duration::from_millis(300);
        let mut last_note = String::new();
        let started = Instant::now();
        // 任务列表刚创建时可能还是空的；先给 CD2 一点时间建任务，
        // 避免每个文件都立刻多发一次 FindFileByPath（列查询在 115 侧可能计配额）。
        let find_grace = Duration::from_secs(1);

        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(DestError::Other("已取消".to_string()));
            }

            let tasks = match self.client.upload_tasks(temp_path).await {
                Ok(t) => t,
                Err(Cd2Error::PermissionDenied(m)) => {
                    return Err(DestError::Permission(format!(
                        "令牌缺少「查看传输任务」权限，无法确认 CD2 是否已把文件传到 115。\
                         到 CD2 网页 → 令牌管理 → 编辑该令牌 → 勾选「查看传输任务」后重试。（{m}）"
                    )));
                }
                Err(e) => return Err(e.into_dest()),
            };
            let mut terminal: Option<(UploadState, Option<u64>)> = None;
            let mut last_error = String::new();
            for task in &tasks {
                match Cd2Client::task_state(task) {
                    UploadState::Failed => {
                        last_error = task.error_message.clone();
                        if is_risk_control(&last_error) {
                            return Err(DestError::RiskControl(last_error));
                        }
                    }
                    UploadState::Stopped => {
                        last_error = if task.error_message.is_empty() {
                            "上传任务被取消或暂停".to_string()
                        } else {
                            task.error_message.clone()
                        };
                    }
                    state @ (UploadState::Finished | UploadState::Skipped) => {
                        terminal = Some((state, Some(task.size)));
                    }
                    UploadState::Running => {}
                }
            }
            if let Some(done) = terminal {
                return Ok(done);
            }
            if !last_error.is_empty() && tasks.iter().all(|t| {
                matches!(
                    Cd2Client::task_state(t),
                    UploadState::Failed | UploadState::Stopped
                )
            }) {
                return Err(DestError::Transient(format!("CD2 上传失败: {last_error}")));
            }

            // 任务列表里查不到（秒传完成得很快，任务被清掉）时，用定点查询确认文件已完整
            if tasks.is_empty() && started.elapsed() >= find_grace {
                if let Some(entry) = self.client.find(temp_path).await.map_err(|e| e.into_dest())? {
                    if entry.size.max(0) as u64 == expected_len {
                        return Ok((UploadState::Finished, Some(entry.size.max(0) as u64)));
                    }
                }
            }

            // 把观察到的最新状态记一行日志（状态或进度变化超过 10% 才记，避免刷屏）
            let note = match tasks.iter().find(|t| t.dest_path == temp_path) {
                Some(t) => {
                    let pct = if t.size > 0 {
                        t.transfered_bytes.saturating_mul(100) / t.size
                    } else {
                        0
                    };
                    format!("{}({}%)", t.status, pct)
                }
                None => "等待 CD2 建立上传任务".to_string(),
            };
            if note != last_note {
                tracing::debug!(path = temp_path, state = %note, "CD2 上传状态");
                last_note = note;
            }

            if Instant::now() > deadline {
                return Err(DestError::Transient(format!(
                    "等待 CD2 上传完成超时（已发送 {expected_len} 字节）"
                )));
            }
            tokio::time::sleep(interval).await;
            interval = (interval * 2).min(poll_max);
        }
    }

    /// 等 CD2 传完 → 校验 → 改成正式名。
    async fn finish_upload(
        &self,
        temp_path: &str,
        dst_path: &str,
        expect_existing: bool,
        sent: u64,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<UploadOutcome, DestError> {
        // 等 CD2 传到 115，同时拿到上传任务回报的最终大小
        let (state, remote_size) = self.wait_upload_done(temp_path, sent, cancel).await?;

        // 任务没给出可靠的大小（0 与非空文件不匹配）或任务是 Skipped 时，才补一次定点查询。
        let size_unknown = remote_size.map(|s| s == 0 && sent > 0).unwrap_or(true);
        if size_unknown || matches!(state, UploadState::Skipped) {
            let entry = self
                .client
                .find(temp_path)
                .await
                .map_err(|e| e.into_dest())?
                .ok_or_else(|| {
                    DestError::Transient(format!("上传完成后找不到临时文件: {temp_path}"))
                })?;
            if entry.size.max(0) as u64 != sent {
                return Err(DestError::Transient(format!(
                    "大小校验不通过：本地 {sent}，远端 {}",
                    entry.size
                )));
            }
        } else if let Some(size) = remote_size {
            // 正常情况下只信上传任务回报的大小，不再发 FindFileByPath
            if size != sent {
                return Err(DestError::Transient(format!(
                    "大小校验不通过：本地 {sent}，上传任务回报 {size}"
                )));
            }
        }

        // 发布正式名
        self.publish(temp_path, dst_path, expect_existing, sent).await?;

        Ok(UploadOutcome { size: sent as i64 })
    }

    /// 发布：把临时文件改成正式名。必要时先删旧版本。
    async fn publish(
        &self,
        temp_path: &str,
        dst_path: &str,
        expect_existing: bool,
        size: u64,
    ) -> Result<(), DestError> {
        let (dir, name) = split_path(dst_path);
        if expect_existing {
            match self.client.delete(dst_path).await {
                Ok(()) | Err(Cd2Error::NotFound(_)) => {}
                Err(e) => return Err(e.into_dest()),
            }
        }
        match self.client.rename(temp_path, &name).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // 目标已存在（expect_existing 判断有误，或并发）：
                // 大小一致就认领，否则删掉再改名一次。
                if let Some(entry) = self.client.find(&join_path(&dir, &name)).await.map_err(|e| e.into_dest())? {
                    if entry.size.max(0) as u64 == size {
                        self.client.delete(temp_path).await.map_err(|e| e.into_dest())?;
                        return Ok(());
                    }
                    self.client.delete(&join_path(&dir, &name)).await.map_err(|e2| e2.into_dest())?;
                    self.client.rename(temp_path, &name).await.map_err(|e2| e2.into_dest())?;
                    return Ok(());
                }
                Err(e.into_dest())
            }
        }
    }
}

#[async_trait]
impl Destination for Cd2Destination {
    async fn probe(&self) -> Result<ProbeReport, DestError> {
        let system = self.client.system_info().await.map_err(|e| e.into_dest())?;
        if !system.system_ready {
            return Err(DestError::Other(format!(
                "CD2 未就绪: {}",
                system.system_message.clone().unwrap_or_default()
            )));
        }

        let runtime = self.client.runtime_info().await.ok();
        let mut permissions = None;
        let mut token_root = None;
        let mut token_info_error = None;
        if let Some(token) = &self.token {
            match self.client.token_info(token).await {
                Ok(info) => {
                    token_root = if info.root_dir.is_empty() {
                        None
                    } else {
                        Some(info.root_dir.clone())
                    };
                    if let Some(p) = &info.permissions {
                        permissions = Some(PermissionSummary {
                            list: p.allow_list,
                            create_folder: p.allow_create_folder,
                            create_file: p.allow_create_file,
                            write: p.allow_write,
                            rename: p.allow_rename,
                            delete: p.allow_delete,
                            transfer_tasks: p.allow_get_transfer_tasks,
                        });
                    }
                }
                Err(e) => token_info_error = Some(e.to_string()),
            }
        }

        // 缺的权限：先从令牌信息里算，再用一次真实的「查传输任务」调用兜底
        // （令牌信息接口不一定可用，但上传完成与否必须靠任务列表判断）
        let mut missing: Vec<String> = permissions
            .as_ref()
            .map(|p| p.missing().iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        match self.client.upload_tasks("__permission_probe__").await {
            Ok(_) => {}
            Err(Cd2Error::PermissionDenied(_)) => {
                if !missing.iter().any(|m| m == "查看传输任务") {
                    missing.push("查看传输任务".to_string());
                }
            }
            Err(_) => {}
        }

        let root_entry = self.client.find(&self.root).await.map_err(|e| e.into_dest())?;
        let root_exists = root_entry.as_ref().map(|f| f.is_directory).unwrap_or(false);
        let space = self.client.space(&self.root).await.ok().map(|s| SpaceView {
            total: s.total_space,
            used: s.used_space,
            free: s.free_space,
        });

        Ok(ProbeReport {
            product: runtime.as_ref().map(|r| r.product_name.clone()).unwrap_or_default(),
            version: runtime.as_ref().map(|r| r.product_version.clone()).unwrap_or_default(),
            cloud_api_version: runtime
                .as_ref()
                .map(|r| r.cloud_api_version.clone())
                .unwrap_or_default(),
            root_exists,
            root_path: self.root.clone(),
            permissions,
            token_root,
            space,
            missing,
            token_info_error,
        })
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<RemoteEntry>, DestError> {
        let files = self
            .client
            .list_sub_files(path, false)
            .await
            .map_err(|e| e.into_dest())?;
        Ok(files.iter().map(to_remote_entry).collect())
    }

    async fn stat(&self, path: &str) -> Result<Option<RemoteEntry>, DestError> {
        Ok(self
            .client
            .find(path)
            .await
            .map_err(|e| e.into_dest())?
            .as_ref()
            .map(to_remote_entry))
    }

    async fn ensure_dir(&self, path: &str) -> Result<(), DestError> {
        self.client.ensure_dir(path).await.map_err(|e| e.into_dest())
    }

    async fn upload(&self, req: UploadRequest<'_>) -> Result<UploadOutcome, DestError> {
        let (dir, name) = split_path(req.dst_path);
        let temp_name = format!("{name}.bmpart");
        let temp_path = join_path(&dir, &temp_name);

        // 1. 建临时文件；父目录可能刚被删掉，补建一次再试
        let handle = match self.client.create_file(&dir, &temp_name).await {
            Ok(h) => h,
            Err(Cd2Error::NotFound(msg)) => {
                self.client.ensure_dir(&dir).await.map_err(|e| e.into_dest())?;
                self.client
                    .create_file(&dir, &temp_name)
                    .await
                    .map_err(|e| {
                        let _ = msg;
                        e.into_dest()
                    })?
            }
            Err(e) => return Err(e.into_dest()),
        };

        // 2. 写数据
        let sent = self
            .client
            .write_stream(
                handle,
                req.reader,
                self.policy.upload_immediately,
                req.progress.clone(),
                req.cancel.clone(),
            )
            .await
            .map_err(|e| e.into_dest())?;

        // 2b. 已知长度时，先核对实际发送的字节数
        if let Some(expected) = req.expected_len {
            if expected != sent {
                return Err(DestError::Transient(format!(
                    "发送字节数不匹配：预计 {expected}，实际 {sent}"
                )));
            }
        }

        // 3~5 步包起来：任何失败都清理临时文件（风控命中时不再发请求）
        let result = self
            .finish_upload(&temp_path, req.dst_path, req.expect_existing, sent, &req.cancel)
            .await;
        if let Err(e) = &result {
            if !matches!(e, DestError::RiskControl(_)) {
                let _ = self.client.delete(&temp_path).await;
            }
        }
        result
    }

    async fn delete(&self, path: &str) -> Result<(), DestError> {
        match self.client.delete(path).await {
            Ok(()) | Err(Cd2Error::NotFound(_)) => Ok(()),
            Err(e) => Err(e.into_dest()),
        }
    }

    async fn read_small(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>, DestError> {
        let info = self
            .client
            .download_url_path(path)
            .await
            .map_err(|e| e.into_dest())?;
        if info.direct_url.is_some() {
            // 要求 get_direct_url=false 后理论上不会出现；出现就说明 CD2 版本行为不同，
            // 这里仍用 CD2 自己的 /static 路径，避免直接去 115。
        }
        let url = super::client::assemble_download_url(self.client.address(), &info);
        http_get_limited(&url, max_bytes).await
    }
}

/// 通过 CD2 的 /static 端点读回内容（不碰 115 直链）。
async fn http_get_limited(url: &str, max_bytes: usize) -> Result<Vec<u8>, DestError> {
    use http_body_util::{BodyExt, Empty};
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    let uri: hyper::Uri = url
        .parse()
        .map_err(|e| DestError::Other(format!("下载地址不合法: {e}")))?;
    let client = Client::builder(TokioExecutor::new()).build_http::<Empty<bytes::Bytes>>();
    let resp = client
        .get(uri)
        .await
        .map_err(|e| DestError::Transient(format!("连接 CD2 下载地址失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(DestError::Transient(format!(
            "CD2 下载失败: HTTP {}",
            resp.status()
        )));
    }
    let mut body = resp.into_body();
    let mut out: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| DestError::Transient(format!("读取下载内容失败: {e}")))?;
        if let Some(data) = frame.data_ref() {
            if out.len() + data.len() > max_bytes {
                return Err(DestError::Other(format!(
                    "文件超过读取上限（{max_bytes} 字节），自检只读小文件"
                )));
            }
            out.extend_from_slice(data);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 根路径必须以斜杠开头() {
        let secret = DestSecret::Token { token: "t".into() };
        let policy = Policy::default();
        assert!(Cd2Destination::new("http://x:1", secret.clone(), "115open", &policy).is_err());
        assert!(Cd2Destination::new("http://x:1", secret, "/115open/", &policy).is_ok());
    }
}
