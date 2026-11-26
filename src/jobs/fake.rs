// 测试用的假目的地：在内存里模拟一个云盘。
//
// 只用来验证任务逻辑（增量判断、首次认领、同步删除、失败中止、保留份数），
// 不涉及任何网络。真实的 CD2 接口由「测试连接 / 自检」在实机上验证。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use crate::destination::{
    DestError, Destination, PermissionSummary, ProbeReport, RemoteEntry, UploadOutcome,
    UploadRequest, split_path,
};

#[derive(Default)]
pub struct FakeDest {
    files: Mutex<HashMap<String, Vec<u8>>>,
    dirs: Mutex<HashSet<String>>,
    /// 接下来多少次上传要失败
    pub fail_uploads: Mutex<usize>,
    /// 命中这个路径时返回风控错误
    pub risk_on: Mutex<Option<String>>,
    /// 接下来多少次删除要失败
    pub fail_deletes: Mutex<usize>,
    pub uploads: Mutex<Vec<String>>,
    pub deletes: Mutex<Vec<String>>,
    pub list_calls: AtomicU64,
}

impl FakeDest {
    pub fn new() -> FakeDest {
        let mut dirs = HashSet::new();
        dirs.insert("/".to_string());
        FakeDest {
            files: Mutex::new(HashMap::new()),
            dirs: Mutex::new(dirs),
            ..Default::default()
        }
    }

    /// 预置一个远端文件（模拟 115 上已有的内容）。
    pub fn put(&self, path: &str, content: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), content.to_vec());
        let (parent, _) = split_path(path);
        self.ensure_parents(&parent);
    }

    fn ensure_parents(&self, path: &str) {
        let mut dirs = self.dirs.lock().unwrap();
        dirs.insert("/".to_string());
        let mut cur = path.trim_end_matches('/').to_string();
        while !cur.is_empty() && cur != "/" {
            dirs.insert(cur.clone());
            cur = split_path(&cur).0;
        }
    }

    pub fn exists(&self, path: &str) -> bool {
        self.files.lock().unwrap().contains_key(path) || self.dirs.lock().unwrap().contains(path)
    }

    pub fn content(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned()
    }

    pub fn upload_count(&self) -> usize {
        self.uploads.lock().unwrap().len()
    }

    pub fn delete_count(&self) -> usize {
        self.deletes.lock().unwrap().len()
    }

    pub fn list_count(&self) -> u64 {
        self.list_calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Destination for FakeDest {
    async fn probe(&self) -> Result<ProbeReport, DestError> {
        Ok(ProbeReport {
            product: "FakeCloudDrive".to_string(),
            version: "test".to_string(),
            cloud_api_version: "test".to_string(),
            root_exists: true,
            root_path: "/115open".to_string(),
            permissions: Some(PermissionSummary {
                list: true,
                create_folder: true,
                create_file: true,
                write: true,
                rename: true,
                delete: true,
                transfer_tasks: true,
            }),
            token_root: None,
            space: None,
            missing: Vec::new(),
            token_info_error: None,
        })
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<RemoteEntry>, DestError> {
        self.list_calls.fetch_add(1, Ordering::Relaxed);
        let path = path.trim_end_matches('/');
        let mut out = Vec::new();
        for (p, content) in self.files.lock().unwrap().iter() {
            if split_path(p).0 == path {
                out.push(RemoteEntry {
                    name: split_path(p).1,
                    is_dir: false,
                    size: content.len() as i64,
                    sha1: None,
                    write_time: None,
                });
            }
        }
        for d in self.dirs.lock().unwrap().iter() {
            if d != "/" && split_path(d).0 == path {
                out.push(RemoteEntry {
                    name: split_path(d).1,
                    is_dir: true,
                    size: 0,
                    sha1: None,
                    write_time: None,
                });
            }
        }
        Ok(out)
    }

    async fn stat(&self, path: &str) -> Result<Option<RemoteEntry>, DestError> {
        if let Some(content) = self.files.lock().unwrap().get(path) {
            return Ok(Some(RemoteEntry {
                name: split_path(path).1,
                is_dir: false,
                size: content.len() as i64,
                sha1: None,
                write_time: None,
            }));
        }
        if self.dirs.lock().unwrap().contains(path) {
            return Ok(Some(RemoteEntry {
                name: split_path(path).1,
                is_dir: true,
                size: 0,
                sha1: None,
                write_time: None,
            }));
        }
        Ok(None)
    }

    async fn ensure_dir(&self, path: &str) -> Result<(), DestError> {
        self.ensure_parents(path.trim_end_matches('/'));
        Ok(())
    }

    async fn upload(&self, mut req: UploadRequest<'_>) -> Result<UploadOutcome, DestError> {
        if let Some(pattern) = self.risk_on.lock().unwrap().clone() {
            if req.dst_path.contains(&pattern) {
                return Err(DestError::RiskControl("770004".to_string()));
            }
        }
        {
            let mut fail = self.fail_uploads.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err(DestError::Transient("模拟上传失败".to_string()));
            }
        }
        let mut content = Vec::new();
        req.reader
            .read_to_end(&mut content)
            .await
            .map_err(|e| DestError::Other(e.to_string()))?;
        if let Some(expected) = req.expected_len {
            if expected != content.len() as u64 {
                return Err(DestError::Other("长度不一致".to_string()));
            }
        }
        let (dir, _) = split_path(req.dst_path);
        self.ensure_parents(&dir);
        self.files
            .lock()
            .unwrap()
            .insert(req.dst_path.to_string(), content.clone());
        self.uploads.lock().unwrap().push(req.dst_path.to_string());
        Ok(UploadOutcome {
            size: content.len() as i64,
        })
    }

    async fn delete(&self, path: &str) -> Result<(), DestError> {
        {
            let mut fail = self.fail_deletes.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err(DestError::Transient("模拟删除失败".to_string()));
            }
        }
        let mut files = self.files.lock().unwrap();
        if files.remove(path).is_some() {
            drop(files);
            self.deletes.lock().unwrap().push(path.to_string());
            return Ok(());
        }
        drop(files);
        let mut dirs = self.dirs.lock().unwrap();
        if dirs.remove(path) {
            drop(dirs);
            self.deletes.lock().unwrap().push(path.to_string());
            return Ok(());
        }
        Err(DestError::NotFound(path.to_string()))
    }

    async fn read_small(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>, DestError> {
        match self.content(path) {
            Some(c) if c.len() <= max_bytes => Ok(c),
            Some(_) => Err(DestError::Other("超过读取上限".to_string())),
            None => Err(DestError::NotFound(path.to_string())),
        }
    }
}

/// 测试用的运行上下文：日志写到临时文件。
pub fn test_ctx(policy: crate::model::Policy) -> (crate::jobs::RunCtx, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.log");
    let file = std::fs::File::create(&path).unwrap();
    let ctx = crate::jobs::RunCtx::new(
        1,
        "测试任务",
        policy,
        Arc::new(AtomicBool::new(false)),
        file,
    );
    (ctx, dir)
}

/// 快速失败策略：连续失败 2 次就停，间隔都取 0，测试跑得快。
pub fn fast_policy() -> crate::model::Policy {
    let mut p = crate::model::Policy::default();
    p.file_gap_ms = 0;
    p.list_gap_ms = 0;
    p.delete_gap_ms = 0;
    p.rpc_gap_ms = 0;
    p.max_failures = 2;
    p
}
