// CD2 客户端：所有对 115 的读写都经由这里调用 CD2 的 gRPC 接口。
//
// 设计要点：
//   - 请求闸门（Gate）在全局串行化调用，并保证最小间隔，避免把 115 的
//     列目录/上传配额打满（见项目文档里的 770004 风控记录）。
//   - 鉴权用 CD2 的 API Token（推荐）或账号密码换 JWT；令牌失效自动重取一次。
//   - 错误按「风控 / 临时 / 权限 / 不存在」分类，交给运行器决定继续还是中止。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::codegen::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Response, Status};

use crate::destination::{DestError, split_path};
use crate::model::{DestSecret, Policy};


// CD2 的生成的客户端：包了一层「自动加 Authorization」的拦截器。
#[allow(clippy::type_complexity)]
pub type Stub = cloud_drive_file_srv_client::CloudDriveFileSrvClient<
    InterceptedService<Channel, AuthInterceptor>,
>;

// 生成代码里的 pb 模块（main.rs 里用 include_proto! 引入）。
use crate::pb;
use crate::pb::cloud_drive_file::HashType;
use crate::pb::cloud_drive_file_srv_client;

/// CD2 里 115 的文件内容 SHA1 在 fileHashes 里的键（HashType::Sha1 = 2）。
pub const HASH_KEY_SHA1: u32 = HashType::Sha1 as u32;

/// 一次上传最多 1 MiB，避开 gRPC 默认消息上限。
pub const CHUNK_BYTES: usize = 1024 * 1024;

/// CD2 调用错误。
#[derive(Debug, Clone)]
pub enum Cd2Error {
    Unauthenticated(String),
    PermissionDenied(String),
    NotFound(String),
    Unimplemented(String),
    Transient(String),
    RiskControl(String),
    Other(String),
}

impl Cd2Error {
    pub fn message(&self) -> &str {
        match self {
            Cd2Error::Unauthenticated(m)
            | Cd2Error::PermissionDenied(m)
            | Cd2Error::NotFound(m)
            | Cd2Error::Unimplemented(m)
            | Cd2Error::Transient(m)
            | Cd2Error::RiskControl(m)
            | Cd2Error::Other(m) => m,
        }
    }

    pub fn from_status(status: Status) -> Cd2Error {
        let detail = status.message().to_string();
        if is_risk_control(&detail) {
            return Cd2Error::RiskControl(detail);
        }
        match status.code() {
            Code::Unauthenticated => Cd2Error::Unauthenticated(detail),
            Code::PermissionDenied => {
                Cd2Error::PermissionDenied(permission_hint(&detail).unwrap_or(detail))
            }
            Code::NotFound => Cd2Error::NotFound(detail),
            Code::Unimplemented => Cd2Error::Unimplemented(detail),
            Code::Unavailable | Code::DeadlineExceeded | Code::ResourceExhausted | Code::Aborted => {
                Cd2Error::Transient(detail)
            }
            Code::Internal | Code::Unknown => {
                if detail.contains("connection") || detail.contains("transport") || detail.contains("timeout") {
                    Cd2Error::Transient(detail)
                } else {
                    Cd2Error::Other(detail)
                }
            }
            other => Cd2Error::Other(format!("{other:?}: {detail}")),
        }
    }

    pub fn into_dest(self) -> DestError {
        match self {
            Cd2Error::RiskControl(m) => DestError::RiskControl(m),
            Cd2Error::Transient(m) => DestError::Transient(m),
            Cd2Error::NotFound(m) => DestError::NotFound(m),
            Cd2Error::PermissionDenied(m) | Cd2Error::Unauthenticated(m) => DestError::Permission(m),
            Cd2Error::Unimplemented(m) => DestError::Unsupported(m),
            Cd2Error::Other(m) => DestError::Other(m),
        }
    }
}

impl std::error::Error for Cd2Error {}

impl std::fmt::Display for Cd2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

/// 把 CD2 的权限报错翻译成"该去勾哪个权限"的指引。
fn permission_hint(detail: &str) -> Option<String> {
    let d = detail.to_lowercase();
    let name = if d.contains("rename permission") {
        "改名"
    } else if d.contains("delete permission") {
        "删除"
    } else if d.contains("get transfer tasks") {
        "查看传输任务"
    } else if d.contains("create folder") || d.contains("mkdir") {
        "建文件夹"
    } else if d.contains("create file") {
        "建文件"
    } else if d.contains("write permission") {
        "写入"
    } else if d.contains("list permission") {
        "列目录"
    } else if d.contains("token management") {
        "令牌管理（只有这个权限才能查令牌权限清单，备份本身不需要）"
    } else {
        return None;
    };
    Some(format!(
        "令牌缺少「{name}」权限：到 CD2 网页 → 令牌管理 → 编辑该令牌勾上它再重试（原始信息：{detail}）"
    ))
}

/// 判断一段错误文本是不是风控/限流。
///
/// 依据：本项目文档记录的 115 风控码 770004，以及 CD2 转述的 405/频率限制文案。
pub fn is_risk_control(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "770004",
        "风控",
        "访问上限",
        "频繁",
        "过于频繁",
        "频率限制",
        "too many requests",
        "rate limit",
        "rate_limit",
        "429 too many",
        "405 method not allowed",
    ]
    .iter()
    .any(|p| t.contains(p))
}

/// 全局请求闸门：串行 + 最小间隔。
pub struct Gate {
    next_allowed: Mutex<Option<Instant>>,
    gap: Duration,
}

impl Gate {
    fn new(gap_ms: u64) -> Gate {
        Gate {
            next_allowed: Mutex::new(None),
            gap: Duration::from_millis(gap_ms),
        }
    }

    async fn wait(&self) {
        let mut next = self.next_allowed.lock().await;
        let now = Instant::now();
        if let Some(at) = *next {
            if at > now {
                tokio::time::sleep(at - now).await;
            }
        }
        *next = Some(Instant::now() + self.gap);
    }
}

/// 自动给每个请求加 Authorization 头的拦截器。
#[derive(Clone)]
pub struct AuthInterceptor {
    bearer: Arc<RwLock<Option<MetadataValue<tonic::metadata::Ascii>>>>,
}

impl Interceptor for AuthInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        if let Some(value) = self.bearer.read().ok().and_then(|b| b.clone()) {
            req.metadata_mut().insert("authorization", value);
        }
        Ok(req)
    }
}

type CallFut<'a, R> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response<R>, Status>> + Send + 'a>>;

/// CD2 客户端。
pub struct Cd2Client {
    address: String,
    stub: Stub,
    secret: DestSecret,
    juggled: Arc<RwLock<Option<MetadataValue<tonic::metadata::Ascii>>>>,
    cache: Mutex<Option<CachedAuth>>,
    gate: Gate,
    retries: u32,
}

struct CachedAuth {
    until: Option<Instant>,
}

impl Cd2Client {
    /// 用目的地配置构造客户端。channel 是懒连接的，构造本身不会发请求。
    pub fn new(address: &str, secret: DestSecret, policy: &Policy) -> Result<Cd2Client, Cd2Error> {
        let uri = normalize_address(address);
        let endpoint = Endpoint::from_shared(uri)
            .map_err(|e| Cd2Error::Other(format!("CD2 地址不合法: {e}")))?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(20))
            .tcp_nodelay(true);
        let channel = endpoint.connect_lazy();

        let bearer = Arc::new(RwLock::new(None::<MetadataValue<_>>));
        // API Token 模式：直接把令牌当 Bearer 用，不需要换 JWT。
        if let DestSecret::Token { token } = &secret {
            if !token.trim().is_empty() {
                if let Ok(v) = MetadataValue::try_from(format!("Bearer {token}")) {
                    *bearer.write().unwrap() = Some(v);
                }
            }
        }

        let stub = cloud_drive_file_srv_client::CloudDriveFileSrvClient::with_interceptor(
            channel,
            AuthInterceptor { bearer: bearer.clone() },
        )
        .max_decoding_message_size(64 * 1024 * 1024)
        .max_encoding_message_size(16 * 1024 * 1024);

        Ok(Cd2Client {
            address: address.to_string(),
            stub,
            secret,
            juggled: bearer,
            cache: Mutex::new(None),
            gate: Gate::new(policy.rpc_gap_ms),
            retries: 2,
        })
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// 调用前准备：闸门 + 鉴权。
    async fn prepare(&self) -> Result<(), Cd2Error> {
        self.gate.wait().await;
        self.ensure_auth().await
    }

    async fn ensure_auth(&self) -> Result<(), Cd2Error> {
        let DestSecret::Password { user, password } = &self.secret else {
            return Ok(());
        };
        {
            let cache = self.cache.lock().await;
            if let Some(c) = &*cache {
                if c.until.map(|u| u > Instant::now()).unwrap_or(false) {
                    return Ok(());
                }
            }
        }
        let resp = self
            .get_token_raw(user, password)
            .await
            .map_err(|e| Cd2Error::Transient(format!("CD2 登录请求失败: {e}")))?;
        if !resp.success {
            return Err(Cd2Error::PermissionDenied(format!(
                "CD2 登录失败: {}",
                resp.error_message
            )));
        }
        let value = MetadataValue::try_from(format!("Bearer {}", resp.token))
            .map_err(|_| Cd2Error::Other("CD2 返回的令牌不是合法 HTTP 头".to_string()))?;
        *self.juggled.write().unwrap() = Some(value);
        // 有效期：有 expiration 就用（留 5 分钟余量），否则按 30 分钟缓存
        let until = resp
            .expiration
            .and_then(|t| chrono::DateTime::from_timestamp(t.seconds, 0))
            .map(|dt| {
                let dur = (dt - chrono::Utc::now()).num_seconds().max(0) as u64;
                Instant::now() + Duration::from_secs(dur.saturating_sub(300).max(60))
            })
            .or_else(|| Some(Instant::now() + Duration::from_secs(30 * 60)));
        *self.cache.lock().await = Some(CachedAuth { until });
        Ok(())
    }

    fn invalidate_auth(&self) {
        let mut guard = self.juggled.write().unwrap();
        if matches!(self.secret, DestSecret::Password { .. }) {
            *guard = None;
        }
    }

    /// 统一调用入口：闸门、重试、鉴权失效重取。
    async fn call<T, R, F>(&self, msg: T, timeout: Duration, f: F) -> Result<R, Cd2Error>
    where
        T: Clone + Send + 'static,
        F: Fn(Stub, Request<T>) -> CallFut<'static, R>,
    {
        let mut auth_retried = false;
        let mut attempt = 0u32;
        loop {
            self.prepare().await?;
            let mut req = Request::new(msg.clone());
            req.set_timeout(timeout);
            match f(self.stub.clone(), req).await {
                Ok(resp) => return Ok(resp.into_inner()),
                Err(status) => {
                    let err = Cd2Error::from_status(status);
                    match err {
                        Cd2Error::Unauthenticated(_) if !auth_retried => {
                            auth_retried = true;
                            self.invalidate_auth();
                            continue;
                        }
                        Cd2Error::Transient(_) if attempt < self.retries => {
                            attempt += 1;
                            let backoff = Duration::from_millis(500 * u64::from(attempt) * u64::from(attempt));
                            tokio::time::sleep(backoff).await;
                            continue;
                        }
                        other => return Err(other),
                    }
                }
            }
        }
    }

    /// 公开方法：GetSystemInfo（不需要鉴权）。
    pub async fn system_info(&self) -> Result<pb::CloudDriveSystemInfo, Cd2Error> {
        self.call((), Duration::from_secs(30), |mut stub, req| {
            Box::pin(async move { stub.get_system_info(req).await })
        })
        .await
    }

    /// 公开方法：GetRuntimeInfo（需要令牌权限，但走同一信道）。
    pub async fn runtime_info(&self) -> Result<pb::RuntimeInfo, Cd2Error> {
        self.call((), Duration::from_secs(30), |mut stub, req| {
            Box::pin(async move { stub.get_runtime_info(req).await })
        })
        .await
    }

    async fn get_token_raw(&self, user: &str, password: &str) -> Result<pb::JwtToken, Status> {
        // 注意：本方法在 prepare() 内部被调用，不能再过一次闸门（会自锁）。
        let req = pb::GetTokenRequest {
            user_name: user.to_string(),
            password: password.to_string(),
            totp_code: None,
        };
        let mut stub = self.stub.clone();
        let mut request = Request::new(req);
        request.set_timeout(Duration::from_secs(120));
        stub.get_token(request).await.map(|r| r.into_inner())
    }

    /// 公开方法：按令牌串查令牌自己的权限信息。
    pub async fn token_info(&self, token: &str) -> Result<pb::TokenInfo, Cd2Error> {
        let req = pb::StringValue { value: token.to_string() };
        self.call(req, Duration::from_secs(30), |mut stub, req| {
            Box::pin(async move { stub.get_api_token_info(req).await })
        })
        .await
    }

    /// 管理员接口：创建令牌（用于「一键建令牌」）。
    pub async fn create_token(
        &self,
        req: pb::CreateTokenRequest,
    ) -> Result<pb::TokenInfo, Cd2Error> {
        self.call(req, Duration::from_secs(30), |mut stub, req| {
            Box::pin(async move { stub.create_token(req).await })
        })
        .await
    }

    /// 列一层目录。
    pub async fn list_sub_files(
        &self,
        path: &str,
        force_refresh: bool,
    ) -> Result<Vec<pb::CloudDriveFile>, Cd2Error> {
        let req = pb::ListSubFileRequest {
            path: path.to_string(),
            force_refresh,
            check_expires: None,
        };
        // 服务端流式：先把所有分片收全；初始失败按统一规则重试。
        let mut auth_retried = false;
        let mut attempt = 0u32;
        loop {
            self.prepare().await?;
            let mut request = Request::new(req.clone());
            request.set_timeout(Duration::from_secs(180));
            let mut stub = self.stub.clone();
            match stub.get_sub_files(request).await {
                Ok(resp) => {
                    let mut stream = resp.into_inner();
                    let mut out = Vec::new();
                    loop {
                        match stream.message().await {
                            Ok(Some(reply)) => out.extend(reply.sub_files),
                            Ok(None) => return Ok(out),
                            Err(status) => {
                                let err = Cd2Error::from_status(status);
                                match err {
                                    Cd2Error::Unauthenticated(_) if !auth_retried => {
                                        auth_retried = true;
                                        self.invalidate_auth();
                                        break;
                                    }
                                    other => return Err(other),
                                }
                            }
                        }
                    }
                }
                Err(status) => {
                    let err = Cd2Error::from_status(status);
                    match err {
                        Cd2Error::Unauthenticated(_) if !auth_retried => {
                            auth_retried = true;
                            self.invalidate_auth();
                            continue;
                        }
                        Cd2Error::Transient(_) if attempt < self.retries => {
                            attempt += 1;
                            tokio::time::sleep(Duration::from_millis(800 * u64::from(attempt))).await;
                        }
                        other => return Err(other),
                    }
                }
            }
        }
    }

    /// 按路径查一项。不存在返回 None。
    pub async fn find(&self, path: &str) -> Result<Option<pb::CloudDriveFile>, Cd2Error> {
        let path = path.trim_end_matches('/');
        if path.is_empty() || path == "/" {
            return Ok(None);
        }
        let (parent, name) = split_path(path);
        let req = pb::FindFileByPathRequest { parent_path: parent, path: name };
        let res = self
            .call(req, Duration::from_secs(60), |mut stub, req| {
                Box::pin(async move { stub.find_file_by_path(req).await })
            })
            .await;
        match res {
            Ok(file) => Ok(if file.name.is_empty() { None } else { Some(file) }),
            Err(Cd2Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 建目录（父目录必须已存在）。
    pub async fn create_folder(
        &self,
        parent: &str,
        name: &str,
    ) -> Result<pb::CreateFolderResult, Cd2Error> {
        let req = pb::CreateFolderRequest {
            parent_path: parent.to_string(),
            folder_name: name.to_string(),
        };
        self.call(req, Duration::from_secs(60), |mut stub, req| {
            Box::pin(async move { stub.create_folder(req).await })
        })
        .await
    }

    /// 建空文件并拿到写入句柄。
    pub async fn create_file(&self, parent: &str, name: &str) -> Result<u64, Cd2Error> {
        let req = pb::CreateFileRequest {
            parent_path: parent.to_string(),
            file_name: name.to_string(),
        };
        let res = self
            .call(req, Duration::from_secs(60), |mut stub, req| {
                Box::pin(async move { stub.create_file(req).await })
            })
            .await?;
        Ok(res.file_handle)
    }

    /// 改名（同目录内）。
    pub async fn rename(&self, path: &str, new_name: &str) -> Result<(), Cd2Error> {
        let req = pb::RenameFileRequest {
            the_file_path: path.to_string(),
            new_name: new_name.to_string(),
        };
        let res = self
            .call(req, Duration::from_secs(60), |mut stub, req| {
                Box::pin(async move { stub.rename_file(req).await })
            })
            .await?;
        expect_ok(res, "改名")
    }

    /// 删除（进网盘回收站）。
    pub async fn delete(&self, path: &str) -> Result<(), Cd2Error> {
        let req = pb::FileRequest { path: path.to_string(), force_refresh: None };
        let res = self
            .call(req, Duration::from_secs(60), |mut stub, req| {
                Box::pin(async move { stub.delete_file(req).await })
            })
            .await?;
        expect_ok(res, "删除")
    }

    /// 空间信息。
    pub async fn space(&self, path: &str) -> Result<pb::SpaceInfo, Cd2Error> {
        let req = pb::FileRequest { path: path.to_string(), force_refresh: None };
        self.call(req, Duration::from_secs(30), |mut stub, req| {
            Box::pin(async move { stub.get_space_info(req).await })
        })
        .await
    }

    /// 取下载信息（要求 CD2 自己代理，不取 115 直链）。
    pub async fn download_url_path(&self, path: &str) -> Result<pb::DownloadUrlPathInfo, Cd2Error> {
        let req = pb::GetDownloadUrlPathRequest {
            path: path.to_string(),
            preview: false,
            lazy_read: false,
            get_direct_url: false,
        };
        self.call(req, Duration::from_secs(60), |mut stub, req| {
            Box::pin(async move { stub.get_download_url_path(req).await })
        })
        .await
    }

    /// 查某个目标路径的上传任务（CD2 本地任务列表，读的是 CD2 自己）。
    pub async fn upload_tasks(&self, dest_path: &str) -> Result<Vec<pb::UploadFileInfo>, Cd2Error> {
        let req = pb::GetUploadFileListRequest {
            get_all: false,
            items_per_page: 50,
            page_number: 0,
            filter: dest_path.to_string(),
            status_filter: None,
            operator_type_filter: None,
        };
        let res = self
            .call(req, Duration::from_secs(30), |mut stub, req| {
                Box::pin(async move { stub.get_upload_file_list(req).await })
            })
            .await?;
        Ok(res
            .upload_files
            .into_iter()
            .filter(|t| t.dest_path == dest_path)
            .collect())
    }

    /// 把数据流写给一个已创建的句柄。返回 (已发送字节, 本地算出的 SHA1)。
    ///
    /// 只负责「写」：是否传完由调用方用 upload_tasks / stat 判断。
    pub async fn write_stream(
        &self,
        handle: u64,
        mut src: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        upload_immediately: bool,
        progress: Arc<AtomicU64>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(u64, String), Cd2Error> {
        self.prepare().await?;

        let (tx, rx) = mpsc::channel::<pb::WriteFileRequest>(4);
        let feeder = tokio::spawn(async move {
            let mut buf = vec![0u8; CHUNK_BYTES];
            let mut pos: u64 = 0;
            let mut hasher = Sha1::new();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err::<(u64, String), Cd2Error>(Cd2Error::Other("上传已取消".to_string()));
                }
                let n = match src.read(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => return Err(Cd2Error::Other(format!("读源数据失败: {e}"))),
                };
                let last = n == 0;
                hasher.update(&buf[..n]);
                let req = pb::WriteFileRequest {
                    file_handle: handle,
                    start_pos: pos,
                    length: n as u64,
                    buffer: buf[..n].to_vec(),
                    close_file: last,
                    upload_immediately: if last && upload_immediately { Some(true) } else { None },
                };
                pos += n as u64;
                progress.store(pos, Ordering::Relaxed);
                if tx.send(req).await.is_err() {
                    return Err(Cd2Error::Transient("上传流被对端中断".to_string()));
                }
                if last {
                    break;
                }
            }
            Ok((pos, hex::encode(hasher.finalize())))
        });

        let mut stub = self.stub.clone();
        let resp = stub.write_to_file_stream(ReceiverStream::new(rx)).await;
        let fed = feeder
            .await
            .map_err(|e| Cd2Error::Other(format!("上传任务异常退出: {e}")))?;

        let result = match resp {
            Ok(r) => r.into_inner(),
            Err(status) => return Err(Cd2Error::from_status(status)),
        };
        let (sent, sha1) = fed?;
        if result.bytes_written != sent {
            return Err(Cd2Error::Other(format!(
                "写入字节数不一致：本地 {sent}，CD2 回报 {}",
                result.bytes_written
            )));
        }
        Ok((sent, sha1))
    }

    // ---------- 便捷封装 ----------

    /// 判断路径是否存在且为目录。
    pub async fn dir_exists(&self, path: &str) -> Result<bool, Cd2Error> {
        Ok(self.find(path).await?.map(|f| f.is_directory).unwrap_or(false))
    }

    /// 递归确保目录存在。
    pub async fn ensure_dir(&self, path: &str) -> Result<(), Cd2Error> {
        let path = path.trim_end_matches('/');
        if path.is_empty() || path == "/" {
            return Ok(());
        }
        match self.find(path).await? {
            Some(f) if f.is_directory => return Ok(()),
            Some(_) => {
                return Err(Cd2Error::Other(format!("远端路径已存在但不是目录: {path}")));
            }
            None => {}
        }
        let (parent, name) = split_path(path);
        if parent == path {
            return Err(Cd2Error::Other(format!("无法创建根目录: {path}")));
        }
        Box::pin(self.ensure_dir(&parent)).await?;
        let res = self.create_folder(&parent, &name).await?;
        if let Some(op) = &res.result {
            if !op.success {
                // 并发/缓存场景下可能已存在，再确认一次
                if self.dir_exists(path).await? {
                    return Ok(());
                }
                return Err(Cd2Error::Other(format!(
                    "创建目录 {path} 失败: {}",
                    op.error_message
                )));
            }
        }
        Ok(())
    }

    /// 上传任务列表里解析终态。
    pub fn task_state(task: &pb::UploadFileInfo) -> UploadState {
        use pb::upload_file_info::Status as S;
        match S::try_from(task.status_enum).ok() {
            Some(S::Finish) => UploadState::Finished,
            Some(S::Skipped) | Some(S::Ignored) => UploadState::Skipped,
            Some(S::Error) | Some(S::FatalError) => UploadState::Failed,
            Some(S::Cancelled) | Some(S::Pause) => UploadState::Stopped,
            _ => UploadState::Running,
        }
    }
}

fn expect_ok(res: pb::FileOperationResult, what: &str) -> Result<(), Cd2Error> {
    if res.success {
        Ok(())
    } else {
        Err(Cd2Error::Other(format!("{what}失败: {}", res.error_message)))
    }
}

/// 上传任务状态归类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadState {
    Running,
    Finished,
    Skipped,
    Failed,
    Stopped,
}

/// 补全地址里的协议：允许只填 host:port。
pub fn normalize_address(address: &str) -> String {
    let a = address.trim();
    if a.starts_with("http://") || a.starts_with("https://") {
        a.to_string()
    } else {
        format!("http://{a}")
    }
}

/// 把 CD2 的文件信息转成目的地通用项。
pub fn to_remote_entry(f: &pb::CloudDriveFile) -> crate::destination::RemoteEntry {
    crate::destination::RemoteEntry {
        name: f.name.clone(),
        is_dir: f.is_directory,
        size: f.size,
        sha1: f.file_hashes.get(&HASH_KEY_SHA1).cloned(),
        write_time: f.write_time.as_ref().map(|t| t.seconds),
    }
}

/// 拼出 CD2 自己的下载地址（get_direct_url=false 时返回 /static/... 形式）。
pub fn assemble_download_url(base: &str, info: &pb::DownloadUrlPathInfo) -> String {
    let base = base.trim().trim_end_matches('/');
    let scheme = if base.starts_with("https://") { "https" } else { "http" };
    let host = base
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let path = info
        .download_url_path
        .replace("{SCHEME}", scheme)
        .replace("{HOST}", host)
        .replace("{PREVIEW}", "false");
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 地址补全协议() {
        assert_eq!(normalize_address("192.168.8.114:19798"), "http://192.168.8.114:19798");
        assert_eq!(normalize_address("https://x:19798"), "https://x:19798");
        assert_eq!(normalize_address("  http://x:1  "), "http://x:1");
    }

    #[test]
    fn 风控文本识别() {
        assert!(is_risk_control("115 接口返回 770004"));
        assert!(is_risk_control("操作过于频繁"));
        assert!(is_risk_control("Too Many Requests"));
        assert!(!is_risk_control("file not found"));
    }

    #[test]
    fn 下载地址占位符替换() {
        let info = pb::DownloadUrlPathInfo {
            download_url_path: "/static/{SCHEME}/{HOST}/{PREVIEW}/a/b.mkv?token=x".to_string(),
            expires_in: None,
            direct_url: None,
            user_agent: None,
            additional_headers: Default::default(),
        };
        assert_eq!(
            assemble_download_url("http://192.168.8.114:19798", &info),
            "http://192.168.8.114:19798/static/http/192.168.8.114:19798/false/a/b.mkv?token=x"
        );
    }
}
