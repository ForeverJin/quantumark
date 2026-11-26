// 目的地抽象：任务运行器只依赖这个 trait，后面加别的目的地（另一个云盘、
// 本地目录等）只需要再实现一份并注册。
//
// 首版实现是 cd2::Cd2Destination：所有读写都发给 CD2 的 gRPC API，
// 由 CD2 去访问 115。程序里不存在直连 115 的代码路径。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use serde::Serialize;
use tokio::io::AsyncRead;

/// 目的地操作错误。运行器靠这些分类决定「跳过这个文件」还是「整轮停下」。
#[derive(Debug, Clone)]
pub enum DestError {
    /// 风控/限流（115 的 770004、405 等）。整轮必须立即停。
    RiskControl(String),
    /// 临时性错误（超时、对方不可用）。可以退避后重试。
    Transient(String),
    /// 对象不存在。
    NotFound(String),
    /// 鉴权失败或权限不足。
    Permission(String),
    /// 对方不支持这个操作。
    Unsupported(String),
    /// 其它。
    Other(String),
}

impl DestError {
    pub fn message(&self) -> &str {
        match self {
            DestError::RiskControl(m)
            | DestError::Transient(m)
            | DestError::NotFound(m)
            | DestError::Permission(m)
            | DestError::Unsupported(m)
            | DestError::Other(m) => m,
        }
    }

    /// 是否必须中止整轮。
    pub fn abort_run(&self) -> bool {
        matches!(self, DestError::RiskControl(_))
    }
}

impl std::fmt::Display for DestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = match self {
            DestError::RiskControl(_) => "风控/限流",
            DestError::Transient(_) => "临时错误",
            DestError::NotFound(_) => "不存在",
            DestError::Permission(_) => "权限不足",
            DestError::Unsupported(_) => "不支持",
            DestError::Other(_) => "错误",
        };
        write!(f, "{tag}: {}", self.message())
    }
}

/// 远端一项（文件或目录）。
#[derive(Debug, Clone, Serialize)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: i64,
    /// 远端内容 SHA1（CD2 提供时才有）
    pub sha1: Option<String>,
    /// 修改时间（unix 秒）
    pub write_time: Option<i64>,
}

/// 权限摘要，测试连接时展示。
#[derive(Debug, Clone, Default, Serialize)]
pub struct PermissionSummary {
    pub list: bool,
    pub create_folder: bool,
    pub create_file: bool,
    pub write: bool,
    pub rename: bool,
    pub delete: bool,
    pub transfer_tasks: bool,
}

impl PermissionSummary {
    /// 缺少哪些必需权限（给用户看的提示）。
    pub fn missing(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if !self.list {
            v.push("列目录");
        }
        if !self.create_folder {
            v.push("建文件夹");
        }
        if !self.create_file {
            v.push("建文件");
        }
        if !self.rename {
            v.push("改名");
        }
        if !self.delete {
            v.push("删除");
        }
        if !self.transfer_tasks {
            v.push("查看传输任务");
        }
        v
    }
}

/// 空间信息。
#[derive(Debug, Clone, Serialize)]
pub struct SpaceView {
    pub total: i64,
    pub used: i64,
    pub free: i64,
}

/// 「测试连接」的结果。
#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    pub product: String,
    pub version: String,
    pub cloud_api_version: String,
    /// CD2 里 115 的根路径是否存在且是目录
    pub root_exists: bool,
    pub root_path: String,
    pub permissions: Option<PermissionSummary>,
    /// 令牌被限制的根目录（没有限制为空）
    pub token_root: Option<String>,
    pub space: Option<SpaceView>,
    /// 探测时发现缺的必需权限（含仅靠令牌信息看不出来的项）
    pub missing: Vec<String>,
    /// 查令牌信息失败时的原因（不影响其它探测项）
    pub token_info_error: Option<String>,
}

/// 一次上传请求。
pub struct UploadRequest<'a> {
    /// CD2 命名空间里的目标文件路径（不是目录）
    pub dst_path: &'a str,
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    /// 已知总长度（流水线来源未知时为 None）
    pub expected_len: Option<u64>,
    /// 目的地是否已存在同名文件（由索引判断；影响是否先删旧再改名）
    pub expect_existing: bool,
    /// 已发送字节数（运行器据此打进度日志）
    pub progress: Arc<std::sync::atomic::AtomicU64>,
    /// 取消标志
    pub cancel: Arc<AtomicBool>,
}

/// 上传结果。
#[derive(Debug, Clone)]
pub struct UploadOutcome {
    pub size: i64,
}

/// 目的地。
#[async_trait]
pub trait Destination: Send + Sync {
    /// 连接测试：版本、权限、根路径、空间。
    async fn probe(&self) -> Result<ProbeReport, DestError>;

    /// 列一层目录（不递归）。
    async fn list_dir(&self, path: &str) -> Result<Vec<RemoteEntry>, DestError>;

    /// 查单个路径。不存在返回 Ok(None)。
    async fn stat(&self, path: &str) -> Result<Option<RemoteEntry>, DestError>;

    /// 确保目录存在（递归创建）。
    async fn ensure_dir(&self, path: &str) -> Result<(), DestError>;

    /// 上传一个文件：写临时名 → 等真正传完 → 校验 → 改名为正式名。
    async fn upload(&self, req: UploadRequest<'_>) -> Result<UploadOutcome, DestError>;

    /// 删除路径（目录或文件）。
    async fn delete(&self, path: &str) -> Result<(), DestError>;

    /// 读回一个小文件（自检用；超过 max_bytes 报错）。
    async fn read_small(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>, DestError>;
}

/// 拼接 CD2 路径。parent 的末尾斜杠会被吃掉。
pub fn join_path(parent: &str, name: &str) -> String {
    format!("{}/{}", parent.trim_end_matches('/'), name)
}

/// 拆出 (父目录, 末级名)。CD2 的 FindFileByPath 需要这个形式。
pub fn split_path(path: &str) -> (String, String) {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => ("/".to_string(), trimmed[1..].to_string()),
        Some(i) => (trimmed[..i].to_string(), trimmed[i + 1..].to_string()),
        None => ("/".to_string(), trimmed.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 路径拼接与拆分() {
        assert_eq!(join_path("/115open", "备份"), "/115open/备份");
        assert_eq!(join_path("/115open/a/", "b/c.mkv"), "/115open/a/b/c.mkv");
        assert_eq!(split_path("/115open/a/b.mkv"), ("/115open/a".into(), "b.mkv".into()));
        assert_eq!(split_path("/b.mkv"), ("/".into(), "b.mkv".into()));
        assert_eq!(split_path("/115open/dir/"), ("/115open".into(), "dir".into()));
    }

    #[test]
    fn 风控错误要能中止整轮() {
        assert!(DestError::RiskControl("770004".into()).abort_run());
        assert!(!DestError::Transient("timeout".into()).abort_run());
    }
}
