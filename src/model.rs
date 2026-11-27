// 数据模型：任务、目的地、运行记录、限速策略。
//
// 落库时用 *_row 结构（secret 是密文）；对外返回时用不带 secret 的结构。

use serde::{Deserialize, Serialize};

/// 任务类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// 目录镜像：增量上传，可选同步删除。
    Mirror,
    /// 目录归档：每次运行打一个包。
    Archive,
    /// MySQL / MariaDB 逻辑转储。
    Mysql,
}

impl JobKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobKind::Mirror => "mirror",
            JobKind::Archive => "archive",
            JobKind::Mysql => "mysql",
        }
    }

    pub fn parse(s: &str) -> Result<JobKind, String> {
        match s {
            "mirror" => Ok(JobKind::Mirror),
            "archive" => Ok(JobKind::Archive),
            "mysql" => Ok(JobKind::Mysql),
            other => Err(format!("未知的任务类型: {other}")),
        }
    }
}

/// 目的地配置（不含凭据）。首版只有 CD2，type 固定 clouddrive2。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DestConfig {
    /// CD2 的地址，如 http://192.168.8.114:19798
    pub address: String,
    /// 115 在 CD2 里的根路径，如 /115open
    pub root: String,
}

/// 目的地凭据。加密后存库，永不回显。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DestSecret {
    /// 推荐：CD2 网页里创建的 API Token。
    Token { token: String },
    /// 兜底：CD2 账号密码（每次 GetToken 换 JWT）。
    Password { user: String, password: String },
}

impl DestSecret {
    /// 打码后的展示用文本。
    pub fn masked(&self) -> String {
        match self {
            DestSecret::Token { token } => format!("Token 已设置 ({})", tail(token)),
            DestSecret::Password { user, .. } => format!("账号密码已设置 ({user})"),
        }
    }
}

fn tail(s: &str) -> String {
    let n = s.chars().count();
    if n <= 4 {
        "****".to_string()
    } else {
        let t: String = s.chars().skip(n - 4).collect();
        format!("****{t}")
    }
}

/// 目的地行（含密文）。
#[derive(Clone, Debug)]
pub struct DestinationRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub config_json: String,
    pub secret_sealed: Option<String>,
    /// 留给后续界面展示
    #[allow(dead_code)]
    pub created_at: i64,
    #[allow(dead_code)]
    pub updated_at: i64,
}

impl DestinationRow {
    pub fn config(&self) -> Result<DestConfig, String> {
        serde_json::from_str(&self.config_json).map_err(|e| format!("目的地配置损坏: {e}"))
    }
}

/// 镜像任务参数。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MirrorOptions {
    /// 容器内的源目录，如 /mnt/source/media
    pub source: String,
    /// 包含规则（为空表示全部）
    #[serde(default)]
    pub include: Vec<String>,
    /// 排除规则（glob，按相对路径和文件名匹配）
    #[serde(default = "default_excludes")]
    pub exclude: Vec<String>,
    /// 源端删除后是否同步删除云端（默认关）
    #[serde(default)]
    pub sync_delete: bool,
    /// 跳过首次认领（强制重传，调试用）
    #[serde(default)]
    pub skip_adopt: bool,
    /// 上传校验通过后删除本地文件（本地不留存）
    #[serde(default)]
    pub delete_local: bool,
    /// 删本地时先移到这个目录（留空 = 直接删除）。必须在源目录之外。
    #[serde(default)]
    pub local_trash: Option<String>,
}

pub fn default_excludes() -> Vec<String> {
    [
        "@eaDir",
        "#recycle",
        ".DS_Store",
        "Thumbs.db",
        "._*",
        "*.tmp",
        "*.part",
        "*.bmpart",
        "qm_push.py",
        "qm_push.ps1",
        "qm_push.command",
        "qm_push.cmd",
        ".qm-delete",
        ".stfolder",
        ".Trash*",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 归档任务参数。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchiveOptions {
    pub source: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// zstd 压缩级别 1-19，默认 3
    #[serde(default = "default_zstd_level")]
    pub zstd_level: i32,
    /// age 公钥（age1...），为空表示不加密
    #[serde(default)]
    pub age_recipient: Option<String>,
}

/// MySQL / MariaDB 备份参数。
///
/// 三种方式：
///   - dump    ：连数据库做逻辑转储（默认，一致性最好，不需要停库）
///   - archive ：把数据目录打成一个包（文件级，建议停库或快照）
///   - mirror  ：按文件增量同步数据目录（文件级）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MysqlOptions {
    /// dump / archive / mirror
    #[serde(default = "default_mysql_mode")]
    pub mode: String,
    // ---- dump 方式 ----
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_mysql_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    /// 空表示所有非系统库
    #[serde(default)]
    pub databases: Vec<String>,
    /// 追加给 mariadb-dump 的参数
    #[serde(default)]
    pub extra_args: Vec<String>,
    // ---- archive / mirror 方式 ----
    /// 数据目录（容器内路径，如 /host/var/lib/mysql）
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    // ---- 通用 ----
    #[serde(default = "default_zstd_level")]
    pub zstd_level: i32,
    #[serde(default)]
    pub age_recipient: Option<String>,
}

/// 任务的敏感附加信息（目前只有数据库密码）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct JobSecret {
    #[serde(default)]
    pub mysql_password: Option<String>,
}

pub fn default_mysql_mode() -> String {
    "dump".to_string()
}

fn default_mysql_port() -> u16 {
    3306
}

fn default_zstd_level() -> i32 {
    3
}

/// 任务行。
#[derive(Clone, Debug)]
pub struct JobRow {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub kind: String,
    pub destination_id: i64,
    pub remote_dir: String,
    pub cron: Option<String>,
    pub keep: Option<i64>,
    pub options_json: String,
    pub secret_sealed: Option<String>,
    /// 留给后续界面展示
    #[allow(dead_code)]
    pub created_at: i64,
    #[allow(dead_code)]
    pub updated_at: i64,
}

impl JobRow {
    pub fn kind(&self) -> Result<JobKind, String> {
        JobKind::parse(&self.kind)
    }
}

/// 运行统计。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunStats {
    /// 源端扫描到的文件数
    pub scanned: u64,
    /// 需要处理的文件数
    pub candidates: u64,
    /// 实际写入（上传）成功数
    pub uploaded: u64,
    pub uploaded_bytes: u64,
    /// 已存在、按校验认领数
    pub adopted: u64,
    /// 跳过（未变化）
    pub skipped: u64,
    /// 删除云端旧文件数
    pub deleted: u64,
    /// 失败文件数
    pub failed: u64,
    /// 上传后清理掉的本地文件数
    #[serde(default)]
    pub local_deleted: u64,
}

impl RunStats {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// 运行记录行。
#[derive(Clone, Debug)]
pub struct RunRow {
    pub id: i64,
    pub job_id: i64,
    pub trigger: String,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub stats_json: Option<String>,
    pub error: Option<String>,
    pub log_path: Option<String>,
}

/// 文件索引行。
#[derive(Clone, Debug)]
pub struct FileRow {
    pub job_id: i64,
    pub rel: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub sha1: Option<String>,
    pub remote_path: String,
    pub updated_at: i64,
}

/// 待清理的临时上传（失败中断的 .bmpart）。
#[derive(Clone, Debug)]
pub struct PendingRow {
    pub id: i64,
    #[allow(dead_code)]
    pub job_id: i64,
    pub remote_temp_path: String,
    #[allow(dead_code)]
    pub created_at: i64,
}

/// 已上传的归档（用于保留份数）。
#[derive(Clone, Debug)]
pub struct ArchiveRow {
    pub id: i64,
    #[allow(dead_code)]
    pub job_id: i64,
    pub remote_path: String,
    #[allow(dead_code)]
    pub size: i64,
    #[allow(dead_code)]
    pub sha1: Option<String>,
    #[allow(dead_code)]
    pub created_at: i64,
}

/// 限速与风控策略，可在网页里改。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Policy {
    /// 两个文件之间的最小间隔（毫秒）
    #[serde(default = "d300")]
    pub file_gap_ms: u64,
    /// 两次列目录之间的最小间隔（毫秒）
    #[serde(default = "d1000")]
    pub list_gap_ms: u64,
    /// 两次删除之间的最小间隔（毫秒）
    #[serde(default = "d1000")]
    pub delete_gap_ms: u64,
    /// 任意两次 CD2 调用之间的最小间隔（毫秒）
    #[serde(default = "d200")]
    pub rpc_gap_ms: u64,
    /// 上传状态轮询间隔（毫秒）
    #[serde(default = "d10000")]
    pub poll_ms: u64,
    /// 连续失败多少次中止本轮
    #[serde(default = "d3")]
    pub max_failures: u32,
    /// 触发风控后的冷却时间（分钟）
    #[serde(default = "d60")]
    pub cooldown_minutes: u64,
    /// 上传时是否要求 CD2 立刻上传（不排队）
    #[serde(default = "dtrue")]
    pub upload_immediately: bool,
    /// 停机错过的定时任务，多久以内补跑（秒）
    #[serde(default = "d86400")]
    pub misfire_window_s: u64,
}

fn d200() -> u64 {
    200
}
fn d300() -> u64 {
    300
}
fn d1000() -> u64 {
    1000
}
fn d10000() -> u64 {
    10000
}
fn d3() -> u32 {
    3
}
fn d60() -> u64 {
    60
}
fn d86400() -> u64 {
    86400
}
fn dtrue() -> bool {
    true
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            file_gap_ms: d300(),
            list_gap_ms: d1000(),
            delete_gap_ms: d1000(),
            rpc_gap_ms: d200(),
            poll_ms: d10000(),
            max_failures: d3(),
            cooldown_minutes: d60(),
            upload_immediately: dtrue(),
            misfire_window_s: d86400(),
        }
    }
}

impl Policy {
    /// 从设置里读，损坏时回落到默认值。
    pub fn from_json_or_default(raw: Option<&str>) -> Policy {
        raw.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 打码只留末尾四位() {
        let s = DestSecret::Token { token: "abcdefgh1234".into() };
        let m = s.masked();
        assert!(m.contains("1234"));
        assert!(!m.contains("abcdefgh"));
    }

    #[test]
    fn 短令牌全部打码() {
        let s = DestSecret::Token { token: "ab".into() };
        assert_eq!(s.masked(), "Token 已设置 (****)");
    }

    #[test]
    fn 策略缺字段用默认值() {
        let p = Policy::from_json_or_default(Some(r#"{"poll_ms":5000}"#));
        assert_eq!(p.poll_ms, 5000);
        assert_eq!(p.file_gap_ms, 300);
        assert!(p.upload_immediately);
    }

    #[test]
    fn 默认排除表非空() {
        assert!(default_excludes().contains(&"@eaDir".to_string()));
    }
}
