// 子进程流水线：mysqldump/tar → zstd → age，直接把最终标准输出交给上传，
// 全程不落临时文件（Armbian 这类小机器的磁盘可能很紧张）。
//
// 任何一段非零退出都算失败，错误里带上各段的 stderr 尾部。

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// 流水线里的一段。
#[derive(Clone, Debug)]
pub struct StageSpec {
    pub program: String,
    pub args: Vec<String>,
    /// 额外的环境变量（如 MYSQL_PWD，避免密码出现在命令行）
    pub env: Vec<(String, String)>,
}

impl StageSpec {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> StageSpec {
        StageSpec { program: program.into(), args, env: Vec::new() }
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> StageSpec {
        self.env.push((key.into(), value.into()));
        self
    }
}

const STDERR_CAP: usize = 16 * 1024;

pub struct Pipeline {
    names: Vec<String>,
    children: Vec<Option<Child>>,
    copies: Vec<JoinHandle<()>>,
    stderr: Vec<Arc<Mutex<Vec<u8>>>>,
    stderr_tasks: Vec<JoinHandle<()>>,
    reader: Option<Box<dyn AsyncRead + Send + Unpin>>,
}

impl Pipeline {
    /// 启动整条流水线。第一段不要自己读标准输入。
    pub fn start(specs: Vec<StageSpec>) -> Result<Pipeline, String> {
        if specs.is_empty() {
            return Err("流水线至少要有一段".to_string());
        }
        let mut children: Vec<Option<Child>> = Vec::with_capacity(specs.len());
        let mut names = Vec::with_capacity(specs.len());
        let mut stderr = Vec::with_capacity(specs.len());

        for (i, spec) in specs.iter().enumerate() {
            let mut cmd = Command::new(&spec.program);
            cmd.args(&spec.args)
                .stdin(if i == 0 { Stdio::null() } else { Stdio::piped() })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            for (k, v) in &spec.env {
                cmd.env(k, v);
            }
            let child = cmd
                .spawn()
                .map_err(|e| format!("启动 {} 失败: {e}", spec.program))?;
            names.push(format!("{} {}", spec.program, spec.args.first().cloned().unwrap_or_default()));
            children.push(Some(child));
            stderr.push(Arc::new(Mutex::new(Vec::new())));
        }

        // 把前一段的 stdout 接到下一段的 stdin
        let mut copies = Vec::new();
        for i in 0..children.len() - 1 {
            let mut out = children[i]
                .as_mut()
                .and_then(|c| c.stdout.take())
                .ok_or_else(|| "流水线内部错误：缺少标准输出".to_string())?;
            let mut next_in = children[i + 1]
                .as_mut()
                .and_then(|c| c.stdin.take())
                .ok_or_else(|| "流水线内部错误：缺少标准输入".to_string())?;
            copies.push(tokio::spawn(async move {
                let _ = tokio::io::copy(&mut out, &mut next_in).await;
                // next_in 在这里 drop，等于给下一段发 EOF
            }));
        }

        // 收集各段 stderr 尾部
        let mut stderr_tasks = Vec::new();
        for (i, child) in children.iter_mut().enumerate() {
            let mut err = match child.as_mut().and_then(|c| c.stderr.take()) {
                Some(e) => e,
                None => continue,
            };
            let sink = stderr[i].clone();
            stderr_tasks.push(tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match err.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut v) = sink.lock() {
                                v.extend_from_slice(&buf[..n]);
                                if v.len() > STDERR_CAP {
                                    let cut = v.len() - STDERR_CAP;
                                    v.drain(..cut);
                                }
                            }
                        }
                    }
                }
            }));
        }

        let reader = children
            .last_mut()
            .and_then(|c| c.as_mut())
            .and_then(|c| c.stdout.take())
            .ok_or_else(|| "流水线内部错误：缺少最终输出".to_string())?;

        Ok(Pipeline {
            names,
            children,
            copies,
            stderr,
            stderr_tasks,
            reader: Some(Box::new(reader)),
        })
    }

    /// 取出最终输出流（只能取一次）。
    pub fn take_reader(&mut self) -> Box<dyn AsyncRead + Send + Unpin> {
        self.reader.take().expect("流水线输出流只能取一次")
    }

    /// 等所有子进程结束，检查退出码。
    pub async fn finish(mut self) -> Result<(), String> {
        for h in self.copies.drain(..) {
            let _ = h.await;
        }
        // 先把各段 stderr 文本取出来，避免后面可变借用和不可变借用打架
        let stderr_texts: Vec<String> = (0..self.stderr.len())
            .map(|i| stderr_text(&self.stderr, i))
            .collect();
        let mut problems = Vec::new();
        for (i, slot) in self.children.iter_mut().enumerate() {
            if let Some(child) = slot.as_mut() {
                match child.wait().await {
                    Ok(status) => {
                        if !status.success() {
                            problems.push(format!(
                                "{} 退出码 {:?}：{}",
                                self.names[i],
                                status.code(),
                                stderr_texts.get(i).cloned().unwrap_or_default()
                            ));
                        }
                    }
                    Err(e) => problems.push(format!("等待 {} 退出失败: {e}", self.names[i])),
                }
            }
        }
        for h in self.stderr_tasks.drain(..) {
            let _ = h.await;
        }
        if problems.is_empty() { Ok(()) } else { Err(problems.join("；")) }
    }

    /// 主动杀掉所有子进程（上传失败时用）。
    pub fn kill(&mut self) {
        for slot in self.children.iter_mut() {
            if let Some(child) = slot.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

fn stderr_text(stderr: &[Arc<Mutex<Vec<u8>>>], i: usize) -> String {
    let text = stderr
        .get(i)
        .and_then(|s| s.lock().ok().map(|v| String::from_utf8_lossy(&v).trim().to_string()))
        .unwrap_or_default();
    if text.is_empty() {
        "(无输出)".to_string()
    } else {
        text.replace('\n', " ")
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.kill();
    }
}

/// 导出程序：优先 mariadb-dump（Debian 的 mariadb-client 带），否则 mysqldump。
pub async fn find_dump_program() -> Result<String, String> {
    for candidate in ["mariadb-dump", "mysqldump"] {
        if program_available(candidate).await {
            return Ok(candidate.to_string());
        }
    }
    Err("容器里找不到 mariadb-dump 或 mysqldump".to_string())
}

/// 查询用的客户端：mariadb 或 mysql。
pub async fn find_mysql_client() -> Result<String, String> {
    for candidate in ["mariadb", "mysql"] {
        if program_available(candidate).await {
            return Ok(candidate.to_string());
        }
    }
    Err("容器里找不到 mariadb 或 mysql 客户端".to_string())
}

/// 检查某个外部命令在容器里是否可用。
pub async fn program_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

