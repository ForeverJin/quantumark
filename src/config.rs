// 配置：统一从环境变量读取，全部带 QM_ 前缀。
//
// 约定（沿用本机其它项目的做法）：
//   - 环境变量集中在这里读取并做基本校验，别处不要直接 std::env::var。
//   - 新增配置要同步 README 的环境变量表和 .env.example。

use std::path::PathBuf;

/// 运行期配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 数据目录：SQLite、密钥、运行日志都在这里。容器里固定 /data。
    pub data_dir: PathBuf,
    /// 网页监听地址。
    pub bind: String,
    /// 首次启动的管理员密码。为空时改用随机设置码（打在日志里）。
    pub admin_password: Option<String>,
    /// 日志级别（RUST_LOG 风格）。
    pub log_level: String,
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            data_dir: PathBuf::from(env_or("QM_DATA_DIR", "/data")),
            bind: env_or("QM_BIND", "0.0.0.0:8790"),
            admin_password: non_empty("QM_ADMIN_PASSWORD"),
            log_level: env_or("QM_LOG", "quantumark=info,tower_http=warn"),
        }
    }

    /// SQLite 文件路径。
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("quantumark.db")
    }

    /// 加解密密钥文件路径。
    pub fn key_path(&self) -> PathBuf {
        self.data_dir.join("secret.key")
    }

    /// 每次运行的日志目录。
    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
}

fn env_or(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => default.to_string(),
    }
}

fn non_empty(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}
