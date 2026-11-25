// JSON 接口实现。

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use super::auth::{hash_password, verify_password};
use super::{AppState, cookie_value};
use crate::cd2::Cd2Destination;
use crate::destination::Destination;
use crate::jobs::mirror;
use crate::model::{
    ArchiveOptions, DestConfig, DestSecret, DestinationRow, JobKind, JobSecret, MirrorOptions,
    MysqlOptions, Policy, RunRow,
};
use crate::runner::get_cooldown;
use crate::store::now;

// ---------- 错误 ----------

pub struct ApiError(String);

impl From<String> for ApiError {
    fn from(value: String) -> Self {
        ApiError(value)
    }
}

impl From<&str> for ApiError {
    fn from(value: &str) -> Self {
        ApiError(value.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": self.0 })),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------- 会话 ----------

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    hex::encode(buf)
}

async fn new_session(state: &AppState) -> Result<(String, String), ApiError> {
    let token = random_token(32);
    let csrf = random_token(16);
    state.store.put_session(&token, &csrf)?;
    Ok((token, csrf))
}

fn session_cookie(token: &str) -> String {
    format!("qm_session={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400")
}

#[derive(Deserialize)]
pub struct LoginReq {
    password: String,
}

pub async fn login(State(state): State<Arc<AppState>>, Json(req): Json<LoginReq>) -> ApiResult<Response> {
    let stored = state
        .store
        .get_setting("admin_password_hash")?
        .unwrap_or_default();
    if stored.trim().is_empty() {
        return Err(ApiError("还没有设置管理员密码，请先完成初始设置".into()));
    }
    if !verify_password(&req.password, &stored) {
        // 失败时的固定延迟，简单挡一下爆破
        tokio::time::sleep(Duration::from_millis(800)).await;
        return Err(ApiError("密码不正确".into()));
    }
    let (token, csrf) = new_session(&state).await?;
    Ok((
        [(header::SET_COOKIE, session_cookie(&token))],
        Json(json!({ "csrf": csrf })),
    )
        .into_response())
}

pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Json<Value> {
    if let Some(token) = cookie_value(&headers, "qm_session") {
        let _ = state.store.drop_session(&token);
    }
    Json(json!({ "ok": true }))
}

pub async fn me(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Json<Value> {
    let csrf = match cookie_value(&headers, "qm_session") {
        Some(token) => state.store.get_session(&token).ok().flatten().map(|(csrf, _)| csrf),
        None => None,
    };
    Json(json!({
        "logged_in": csrf.is_some(),
        "csrf": csrf,
        "needs_setup": state.needs_setup(),
        "setup_code_hint": if state.needs_setup() { state.setup_code.is_some() } else { false },
    }))
}

#[derive(Deserialize)]
pub struct SetupReq {
    code: String,
    password: String,
}

pub async fn setup(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SetupReq>,
) -> ApiResult<Response> {
    if !state.needs_setup() {
        return Err(ApiError("已经设置过管理员密码".into()));
    }
    let ok = match &state.setup_code {
        Some(code) => code == req.code.trim(),
        None => false,
    };
    if !ok {
        return Err(ApiError("设置码不正确（看容器日志里的初始设置码）".into()));
    }
    if req.password.chars().count() < 8 {
        return Err(ApiError("管理员密码至少 8 位".into()));
    }
    let hash = hash_password(&req.password)?;
    state.store.set_setting("admin_password_hash", &hash)?;
    state.store.set_setting("setup_code", "")?;
    let (token, csrf) = new_session(&state).await?;
    Ok((
        [(header::SET_COOKIE, session_cookie(&token))],
        Json(json!({ "csrf": csrf })),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct PasswordReq {
    old: String,
    new: String,
}

pub async fn change_password(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PasswordReq>,
) -> ApiResult<Json<Value>> {
    let stored = state.store.get_setting("admin_password_hash")?.unwrap_or_default();
    if !verify_password(&req.old, &stored) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        return Err(ApiError("原密码不正确".into()));
    }
    if req.new.chars().count() < 8 {
        return Err(ApiError("新密码至少 8 位".into()));
    }
    state
        .store
        .set_setting("admin_password_hash", &hash_password(&req.new)?)?;
    Ok(Json(json!({ "ok": true })))
}

// ---------- 概览 ----------

pub async fn overview(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let jobs = state.store.list_jobs()?;
    let destinations = state.store.list_destinations()?;
    let mut job_views = Vec::new();
    for job in &jobs {
        let last = state.store.list_runs(Some(job.id), 1)?.into_iter().next();
        let next = job
            .cron
            .as_deref()
            .and_then(|c| crate::scheduler::parse(c).ok())
            .and_then(|s| s.after(&chrono::Local::now()).next())
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string());
        job_views.push(json!({
            "id": job.id,
            "name": job.name,
            "kind": job.kind,
            "enabled": job.enabled,
            "destination_id": job.destination_id,
            "remote_dir": job.remote_dir,
            "cron": job.cron,
            "keep": job.keep,
            "running": state.runner.is_active(job.id).await,
            "last_run": last.map(|r| run_view(&r, Some(&job.name))),
            "next_run": next,
        }));
    }
    let cooldown = get_cooldown(&state.store).filter(|t| *t > now());
    Ok(Json(json!({
        "jobs": job_views,
        "destinations": destinations.len(),
        "cooldown_until": cooldown.map(fmt_ts),
        "version": env!("CARGO_PKG_VERSION"),
        "data_dir": state.config.data_dir.display().to_string(),
        "policy": state.store.policy(),
    })))
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn run_view(r: &RunRow, job_name: Option<&str>) -> Value {
    json!({
        "id": r.id,
        "job_id": r.job_id,
        "job_name": job_name,
        "trigger": r.trigger,
        "status": r.status,
        "started_at": r.started_at,
        "started_at_text": fmt_ts(r.started_at),
        "finished_at": r.finished_at,
        "finished_at_text": r.finished_at.map(fmt_ts),
        "stats": r.stats_json.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()),
        "error": r.error,
    })
}

// ---------- 目的地 ----------

fn dest_view(row: &DestinationRow, sealer: &crate::secrets::Sealer) -> Value {
    let cfg = row.config().ok();
    let masked = row
        .secret_sealed
        .as_deref()
        .and_then(|s| sealer.open(s).ok())
        .and_then(|p| serde_json::from_str::<DestSecret>(&p).ok())
        .map(|s| s.masked())
        .unwrap_or_else(|| "未配置".to_string());
    json!({
        "id": row.id,
        "name": row.name,
        "kind": row.kind,
        "address": cfg.as_ref().map(|c| c.address.clone()).unwrap_or_default(),
        "root": cfg.as_ref().map(|c| c.root.clone()).unwrap_or_default(),
        "secret_masked": masked,
        "has_secret": row.secret_sealed.is_some(),
    })
}

pub async fn list_destinations(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let rows = state.store.list_destinations()?;
    Ok(Json(json!({
        "items": rows.iter().map(|r| dest_view(r, &state.sealer)).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
pub struct DestInput {
    name: String,
    address: String,
    root: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

fn build_secret(input: &DestInput) -> Result<Option<DestSecret>, String> {
    let token = input.token.as_deref().unwrap_or("").trim();
    if !token.is_empty() {
        return Ok(Some(DestSecret::Token { token: token.to_string() }));
    }
    let user = input.user.as_deref().unwrap_or("").trim();
    let password = input.password.as_deref().unwrap_or("");
    if !user.is_empty() && !password.is_empty() {
        return Ok(Some(DestSecret::Password {
            user: user.to_string(),
            password: password.to_string(),
        }));
    }
    Ok(None)
}

fn seal_secret<T: serde::Serialize>(
    sealer: &crate::secrets::Sealer,
    secret: &T,
) -> Result<String, String> {
    let json = serde_json::to_string(secret).map_err(|e| format!("凭据序列化失败: {e}"))?;
    sealer.seal(&json)
}

fn validate_dest(input: &DestInput) -> ApiResult<(String, String)> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError("目的地名称不能为空".into()));
    }
    let address = crate::cd2::client::normalize_address(&input.address);
    if address.len() < 10 || (!address.starts_with("http://") && !address.starts_with("https://")) {
        return Err(ApiError("CD2 地址格式不对，例如 http://192.168.8.114:19798".into()));
    }
    let root = input.root.trim().trim_end_matches('/').to_string();
    if !root.starts_with('/') || root.len() < 2 {
        return Err(ApiError("根路径要以 / 开头，例如 /115open".into()));
    }
    Ok((name.to_string(), root))
}

pub async fn create_destination(
    State(state): State<Arc<AppState>>,
    Json(input): Json<DestInput>,
) -> ApiResult<Json<Value>> {
    let (name, root) = validate_dest(&input)?;
    let secret = build_secret(&input).map_err(ApiError)?;
    let sealed = secret
        .as_ref()
        .map(|s| seal_secret(&state.sealer, s))
        .transpose()?;
    let cfg = DestConfig { address: crate::cd2::client::normalize_address(&input.address), root };
    let cfg_json = serde_json::to_string(&cfg).map_err(|e| ApiError(e.to_string()))?;
    let id = state
        .store
        .insert_destination(&name, "clouddrive2", &cfg_json, sealed.as_deref())
        .map_err(friendly_unique)?;
    Ok(Json(json!({ "id": id })))
}

pub async fn update_destination(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(input): Json<DestInput>,
) -> ApiResult<Json<Value>> {
    let (name, root) = validate_dest(&input)?;
    let sealed = match build_secret(&input).map_err(ApiError)? {
        Some(secret) => Some(seal_secret(&state.sealer, &secret)?),
        None => None, // 不改凭据
    };
    let cfg = DestConfig { address: crate::cd2::client::normalize_address(&input.address), root };
    let cfg_json = serde_json::to_string(&cfg).map_err(|e| ApiError(e.to_string()))?;
    state
        .store
        .update_destination(id, &name, &cfg_json, sealed.as_deref())
        .map_err(friendly_unique)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_destination(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    state.store.delete_destination(id)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ProbeInput {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    address: String,
    #[serde(default)]
    root: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

pub async fn probe_unsaved_destination(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ProbeInput>,
) -> ApiResult<Json<Value>> {
    // 优先用已保存的凭据（前端不会拿到明文令牌）
    let (address, root, secret) = if let Some(id) = input.id {
        let row = state
            .store
            .get_destination(id)?
            .ok_or_else(|| ApiError("目的地不存在".into()))?;
        let cfg = row.config().map_err(ApiError)?;
        let secret = decrypt_secret(&state.sealer, &row)?;
        let address = if input.address.trim().is_empty() { cfg.address } else {
            crate::cd2::client::normalize_address(&input.address)
        };
        let root = if input.root.trim().is_empty() { cfg.root } else {
            input.root.trim().trim_end_matches('/').to_string()
        };
        (address, root, secret)
    } else {
        let secret = build_secret(&DestInput {
            name: String::new(),
            address: input.address.clone(),
            root: input.root.clone(),
            token: input.token.clone(),
            user: input.user.clone(),
            password: input.password.clone(),
        })
        .map_err(ApiError)?
        .ok_or_else(|| ApiError("请填写 API Token 或账号密码".into()))?;
        (
            crate::cd2::client::normalize_address(&input.address),
            input.root.trim().trim_end_matches('/').to_string(),
            secret,
        )
    };

    let policy = state.store.policy();
    let dest = Cd2Destination::new(&address, secret, &root, &policy)
        .map_err(|e| ApiError(e.to_string()))?;
    let report = dest.probe().await.map_err(|e| ApiError(e.to_string()))?;
    let missing = report.missing.clone();
    Ok(Json(json!({
        "report": report,
        "missing_permissions": missing,
    })))
}

pub async fn probe_destination(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    probe_unsaved_destination(State(state), Json(ProbeInput {
        id: Some(id),
        address: String::new(),
        root: String::new(),
        token: None,
        user: None,
        password: None,
    }))
    .await
}

fn rec(steps: &mut Vec<Value>, ok: bool, name: &str, detail: impl Into<String>) {
    steps.push(json!({"ok": ok, "name": name, "detail": detail.into()}));
}

/// 自检：往 <根路径>/_selftest 上传一个随机小文件，读回比对，再删掉。
///
/// 覆盖的链路：建立目录 → 流式上传（CD2 收数据后自己传 115）→ 等上传完成 →
/// 定点查询大小和 SHA1 → 经 CD2 的下载地址读回比对 → 删除。
pub async fn selftest_destination(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let row = state
        .store
        .get_destination(id)?
        .ok_or_else(|| ApiError("目的地不存在".into()))?;
    let cfg = row.config().map_err(ApiError)?;
    let dest = destination_for(&state, id).await?;
    let dir = format!("{}/_selftest", cfg.root.trim_end_matches('/'));

    let mut steps: Vec<Value> = Vec::new();
    run_selftest(&dest, &dir, &mut steps).await;
    // 不管成败都把整个自检目录删掉（里面可能有失败残留的 .bmpart）
    if let Err(e) = dest.delete(&dir).await {
        rec(&mut steps, false, "清理自检目录", e.to_string());
    }

    let ok = steps.iter().all(|s| s["ok"].as_bool().unwrap_or(false));
    Ok(Json(json!({"ok": ok, "steps": steps})))
}

/// 自检的各个步骤。失败就停下，清理交给调用方。
async fn run_selftest(dest: &Cd2Destination, dir: &str, steps: &mut Vec<Value>) {
    // 1. 建目录
    if let Err(e) = dest.ensure_dir(dir).await {
        rec(steps, false, "创建自检目录", e.to_string());
        return;
    }
    rec(steps, true, "创建自检目录", dir);

    // 2. 准备随机内容
    let mut payload = vec![0u8; 64 * 1024];
    rand::fill(&mut payload[..]);
    let sha1 = {
        use sha1::Digest;
        hex::encode(sha1::Sha1::digest(&payload))
    };
    let name = format!(
        "selftest-{}.bin",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let dst = crate::destination::join_path(dir, &name);
    let len = payload.len() as u64;

    // 3. 上传（含等待 CD2 传到 115、大小与 SHA1 校验、临时名转正）
    let outcome = dest
        .upload(crate::destination::UploadRequest {
            dst_path: &dst,
            reader: Box::new(std::io::Cursor::new(payload.clone())),
            expected_len: Some(len),
            expect_existing: false,
            progress: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
        .await;
    match outcome {
        Ok(out) => rec(
            steps,
            true,
            "上传并等 CD2 传到 115",
            format!("{} 字节，SHA1 已核对: {}", out.size, out.sha1_verified),
        ),
        Err(e) => {
            rec(steps, false, "上传并等 CD2 传到 115", e.to_string());
            return;
        }
    }

    // 4. 定点查询校验
    match dest.stat(&dst).await {
        Ok(Some(entry)) => {
            let size_ok = entry.size.max(0) as u64 == len;
            let sha_ok = entry
                .sha1
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case(&sha1))
                .unwrap_or(false);
            rec(
                steps,
                size_ok,
                "查询远端大小",
                format!("远端 {} 字节，本地 {} 字节", entry.size, len),
            );
            rec(
                steps,
                sha_ok || entry.sha1.is_none(),
                "比对远端 SHA1",
                match &entry.sha1 {
                    Some(s) => format!("远端 {s}，本地 {sha1}"),
                    None => "远端未提供 SHA1（115 通常会给，缺失不影响备份）".to_string(),
                },
            );
        }
        Ok(None) => rec(steps, false, "查询远端大小", "上传完成后查不到该文件"),
        Err(e) => rec(steps, false, "查询远端大小", e.to_string()),
    }

    // 5. 经 CD2 读回比对
    match dest.read_small(&dst, 4 * 1024 * 1024).await {
        Ok(back) => {
            let same = back == payload;
            rec(
                steps,
                same,
                "经 CD2 读回比对",
                if same {
                    format!("{} 字节完全一致", back.len())
                } else {
                    format!("内容不一致：远端 {} 字节", back.len())
                },
            );
        }
        Err(e) => rec(steps, false, "经 CD2 读回比对", e.to_string()),
    }

    // 6. 删除自检文件
    match dest.delete(&dst).await {
        Ok(()) => rec(steps, true, "删除自检文件", &dst),
        Err(e) => rec(steps, false, "删除自检文件", e.to_string()),
    }
}

fn decrypt_secret(
    sealer: &crate::secrets::Sealer,
    row: &DestinationRow,
) -> Result<DestSecret, String> {
    let sealed = row
        .secret_sealed
        .as_deref()
        .ok_or_else(|| format!("目的地 {} 还没有配置凭据", row.name))?;
    let plain = sealer.open(sealed)?;
    serde_json::from_str(&plain).map_err(|e| format!("目的地凭据损坏: {e}"))
}

async fn destination_for(state: &AppState, id: i64) -> ApiResult<Cd2Destination> {
    let row = state
        .store
        .get_destination(id)?
        .ok_or_else(|| ApiError("目的地不存在".into()))?;
    let cfg = row.config().map_err(ApiError)?;
    let secret = decrypt_secret(&state.sealer, &row)?;
    let policy = state.store.policy();
    Cd2Destination::new(&cfg.address, secret, &cfg.root, &policy)
        .map_err(|e| ApiError(e.to_string()))
}

#[derive(Deserialize)]
pub struct ListQuery {
    path: String,
}

pub async fn list_remote_dir(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let dest = destination_for(&state, id).await?;
    let path = q.path.trim().trim_end_matches('/').to_string();
    let path = if path.is_empty() { "/".to_string() } else { path };
    let entries = dest.list_dir(&path).await.map_err(|e| ApiError(e.to_string()))?;
    let mut items: Vec<Value> = entries
        .iter()
        .map(|e| json!({ "name": e.name, "is_dir": e.is_dir, "size": e.size }))
        .collect();
    items.sort_by_key(|v| v["name"].as_str().unwrap_or("").to_string());
    Ok(Json(json!({ "path": path, "items": items })))
}

#[derive(Deserialize)]
pub struct DeletePathReq {
    path: String,
}

/// 删除目的地根路径下的某个远端目录/文件（进网盘回收站）。
/// 只用于清理：路径必须位于目的地根路径之下，且不能是根路径本身。
pub async fn delete_remote_path(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(req): Json<DeletePathReq>,
) -> ApiResult<Json<Value>> {
    let row = state
        .store
        .get_destination(id)?
        .ok_or_else(|| ApiError("目的地不存在".into()))?;
    let cfg = row.config().map_err(ApiError)?;
    let path = req.path.trim().trim_end_matches('/').to_string();
    if !path.starts_with(&cfg.root) || path == cfg.root || path.len() <= cfg.root.len() + 1 {
        return Err(ApiError(format!(
            "只能删除目的地根路径 {} 之下的子路径（当前: {path}）",
            cfg.root
        )));
    }
    let dest = destination_for(&state, id).await?;
    dest.delete(&path).await.map_err(|e| ApiError(e.to_string()))?;
    tracing::info!(path = %path, "已删除远端路径");
    Ok(Json(json!({"ok": true, "deleted": path})))
}

#[derive(Deserialize)]
pub struct CreateTokenReq {
    address: String,
    user: String,
    password: String,
    root: String,
    #[serde(default)]
    name: Option<String>,
}

/// 用 CD2 账号密码换一个最小权限的 API Token。账号密码只用这一次，不保存。
pub async fn create_cd2_token(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTokenReq>,
) -> ApiResult<Json<Value>> {
    let root = req.root.trim().trim_end_matches('/').to_string();
    if !root.starts_with('/') || root.len() < 2 {
        return Err(ApiError("根路径要以 / 开头，例如 /115open".into()));
    }
    if req.user.trim().is_empty() || req.password.is_empty() {
        return Err(ApiError("需要 CD2 的账号和密码".into()));
    }
    let policy = state.store.policy();
    let client = crate::cd2::Cd2Client::new(
        &req.address,
        DestSecret::Password { user: req.user.trim().to_string(), password: req.password.clone() },
        &policy,
    )
    .map_err(|e| ApiError(e.to_string()))?;

    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("quantumark")
        .to_string();

    let permissions = crate::pb::TokenPermissions {
        allow_list: true,
        allow_create_folder: true,
        allow_create_file: true,
        allow_write: true,
        allow_rename: true,
        allow_delete: true,
        allow_get_space_info: true,
        allow_view_runtime_info: true,
        allow_get_transfer_tasks: true,
        ..Default::default()
    };
    let info = client
        .create_token(crate::pb::CreateTokenRequest {
            root_dir: root,
            permissions: Some(permissions),
            friendly_name: name,
            expires_in: None,
            enable_grpc_log: Some(true),
            enable_stream_file_log: Some(true),
        })
        .await
        .map_err(|e| ApiError(e.to_string()))?;
    Ok(Json(json!({
        "token": info.token,
        "root_dir": info.root_dir,
        "friendly_name": info.friendly_name,
    })))
}

// ---------- 任务 ----------

fn friendly_unique(err: String) -> ApiError {
    if err.contains("UNIQUE") {
        ApiError("名称已被占用，换一个".to_string())
    } else {
        ApiError(err)
    }
}

fn normalize_options(kind: JobKind, value: &Value) -> ApiResult<String> {
    let text = match kind {
        JobKind::Mirror => {
            let opts: MirrorOptions =
                serde_json::from_value(value.clone()).map_err(|e| ApiError(format!("参数不正确: {e}")))?;
            let opts = MirrorOptions { source: opts.source.trim().to_string(), ..opts };
            if opts.source.is_empty() {
                return Err(ApiError("源目录不能为空".into()));
            }
            if opts.delete_local && opts.sync_delete {
                return Err(ApiError(
                    "「上传后删除本地」和「同步删除云端」不能同时开启".into(),
                ));
            }
            if let Some(trash) = opts.local_trash.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                if !trash.starts_with('/') {
                    return Err(ApiError("本地回收目录要写绝对路径".into()));
                }
                let base = format!("{}/", opts.source.trim_end_matches('/'));
                if trash == opts.source.trim_end_matches('/') || trash.starts_with(&base) {
                    return Err(ApiError("本地回收目录不能放在源目录里面".into()));
                }
            }
            serde_json::to_string(&opts)
        }
        JobKind::Archive => {
            let opts: ArchiveOptions =
                serde_json::from_value(value.clone()).map_err(|e| ApiError(format!("参数不正确: {e}")))?;
            let opts = ArchiveOptions { source: opts.source.trim().to_string(), ..opts };
            if opts.source.is_empty() {
                return Err(ApiError("源目录不能为空".into()));
            }
            serde_json::to_string(&opts)
        }
        JobKind::Mysql => {
            let opts: MysqlOptions =
                serde_json::from_value(value.clone()).map_err(|e| ApiError(format!("参数不正确: {e}")))?;
            let mode = if opts.mode.trim().is_empty() {
                "dump".to_string()
            } else {
                opts.mode.trim().to_lowercase()
            };
            let opts = MysqlOptions {
                host: opts.host.trim().to_string(),
                user: opts.user.trim().to_string(),
                source: opts.source.trim().to_string(),
                mode: mode.clone(),
                ..opts
            };
            match mode.as_str() {
                "dump" => {
                    if opts.host.is_empty() || opts.user.is_empty() {
                        return Err(ApiError("连库导出需要填数据库地址和用户名".into()));
                    }
                }
                "archive" | "mirror" => {
                    if opts.source.is_empty() {
                        return Err(ApiError("备份数据目录需要填数据目录路径".into()));
                    }
                }
                _ => {
                    return Err(ApiError(
                        "备份方式只能是 dump（连库导出）/ archive（打包数据目录）/ mirror（同步数据目录）"
                            .into(),
                    ));
                }
            }
            serde_json::to_string(&opts)
        }
    };
    text.map_err(|e| ApiError(e.to_string()))
}

fn validate_remote_dir(state: &AppState, dest_id: i64, remote_dir: &str) -> ApiResult<String> {
    let row = state
        .store
        .get_destination(dest_id)?
        .ok_or_else(|| ApiError("目的地不存在".into()))?;
    let cfg = row.config().map_err(ApiError)?;
    let dir = remote_dir.trim().trim_end_matches('/').to_string();
    if !dir.starts_with('/') || dir.len() < 2 {
        return Err(ApiError("远端目录要以 / 开头，例如 /115open/backup".into()));
    }
    if !dir.starts_with(&cfg.root) {
        return Err(ApiError(format!(
            "远端目录必须在目的地的根路径 {} 之下（当前: {}）",
            cfg.root, dir
        )));
    }
    Ok(dir)
}

#[derive(Deserialize)]
pub struct JobInput {
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    kind: String,
    destination_id: i64,
    remote_dir: String,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    keep: Option<i64>,
    options: Value,
    /// 数据库密码（连库导出用）；留空表示不修改
    #[serde(default)]
    mysql_password: Option<String>,
    /// 清空已保存的数据库密码
    #[serde(default)]
    clear_mysql_password: bool,
}

fn default_true() -> bool {
    true
}

struct PreparedJob {
    name: String,
    enabled: bool,
    kind: JobKind,
    destination_id: i64,
    remote_dir: String,
    cron: Option<String>,
    keep: Option<i64>,
    options_json: String,
    secret_sealed: Option<String>,
}

fn prepare_job(state: &AppState, input: &JobInput) -> ApiResult<PreparedJob> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError("任务名称不能为空".into()));
    }
    let kind = JobKind::parse(&input.kind).map_err(ApiError)?;
    let remote_dir = validate_remote_dir(state, input.destination_id, &input.remote_dir)?;
    let cron = input
        .cron
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(expr) = &cron {
        crate::scheduler::parse(expr).map_err(|e| ApiError(format!("cron 表达式无效: {e}")))?;
    }
    if let Some(keep) = input.keep {
        if !(1..=3650).contains(&keep) {
            return Err(ApiError("保留份数要在 1 到 3650 之间".into()));
        }
    }
    let options_json = normalize_options(kind, &input.options)?;

    // 数据库密码只对「连库导出」有意义，加密存储；留空表示保留旧值
    let secret_sealed = match kind {
        JobKind::Mysql => {
            let is_dump = serde_json::from_str::<MysqlOptions>(&options_json)
                .map(|o| o.mode == "dump")
                .unwrap_or(false);
            if !is_dump {
                None
            } else if input.clear_mysql_password {
                Some(seal_secret(&state.sealer, &JobSecret::default())?)
            } else if let Some(pw) = input.mysql_password.as_deref().filter(|s| !s.is_empty()) {
                Some(seal_secret(
                    &state.sealer,
                    &JobSecret { mysql_password: Some(pw.to_string()) },
                )?)
            } else {
                None
            }
        }
        _ => None,
    };

    Ok(PreparedJob {
        name,
        enabled: input.enabled,
        kind,
        destination_id: input.destination_id,
        remote_dir,
        cron,
        keep: input.keep,
        options_json,
        secret_sealed,
    })
}

pub async fn list_jobs(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let jobs = state.store.list_jobs()?;
    let dests = state.store.list_destinations()?;
    let mut items = Vec::new();
    for job in &jobs {
        let dest_name = dests
            .iter()
            .find(|d| d.id == job.destination_id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "(已删除)".to_string());
        let last = state.store.list_runs(Some(job.id), 1)?.into_iter().next();
        let next = job
            .cron
            .as_deref()
            .and_then(|c| crate::scheduler::parse(c).ok())
            .and_then(|s| s.after(&chrono::Local::now()).next())
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string());
        items.push(json!({
            "id": job.id,
            "name": job.name,
            "enabled": job.enabled,
            "kind": job.kind,
            "destination_id": job.destination_id,
            "destination_name": dest_name,
            "remote_dir": job.remote_dir,
            "cron": job.cron,
            "keep": job.keep,
            "options": serde_json::from_str::<Value>(&job.options_json).unwrap_or(Value::Null),
            "has_secret": job.secret_sealed.is_some(),
            "running": state.runner.is_active(job.id).await,
            "last_run": last.map(|r| run_view(&r, Some(&job.name))),
            "next_run": next,
        }));
    }
    Ok(Json(json!({ "items": items })))
}

pub async fn create_job(
    State(state): State<Arc<AppState>>,
    Json(input): Json<JobInput>,
) -> ApiResult<Json<Value>> {
    let job = prepare_job(&state, &input)?;
    let id = state
        .store
        .insert_job(
            &job.name,
            job.enabled,
            job.kind.as_str(),
            job.destination_id,
            &job.remote_dir,
            job.cron.as_deref(),
            job.keep,
            &job.options_json,
            job.secret_sealed.as_deref(),
        )
        .map_err(friendly_unique)?;
    Ok(Json(json!({ "id": id })))
}

pub async fn update_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(input): Json<JobInput>,
) -> ApiResult<Json<Value>> {
    if state.runner.is_active(id).await {
        return Err(ApiError("任务正在运行，先停止再改".into()));
    }
    let job = prepare_job(&state, &input)?;
    state
        .store
        .update_job(
            id,
            &job.name,
            job.enabled,
            job.destination_id,
            &job.remote_dir,
            job.cron.as_deref(),
            job.keep,
            &job.options_json,
            job.secret_sealed.as_deref(),
        )
        .map_err(friendly_unique)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    if state.runner.is_active(id).await {
        return Err(ApiError("任务正在运行，先停止再删".into()));
    }
    state.store.delete_job(id)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn run_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    state
        .store
        .get_job(id)?
        .ok_or_else(|| ApiError("任务不存在".into()))?;
    state.runner.enqueue(id, "manual").await.map_err(ApiError)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn stop_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    state.runner.stop(id).await.map_err(ApiError)?;
    Ok(Json(json!({ "ok": true })))
}

/// 重置某个任务的索引：下次运行会重新做一次首次认领。
pub async fn reset_index(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    if state.runner.is_active(id).await {
        return Err(ApiError("任务正在运行，先停止".into()));
    }
    state.store.clear_files(id)?;
    state.store.clear_dirs(id)?;
    Ok(Json(json!({ "ok": true })))
}

/// 预览镜像任务会做哪些事（不写任何东西）。
pub async fn dry_run_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let job = state
        .store
        .get_job(id)?
        .ok_or_else(|| ApiError("任务不存在".into()))?;
    let kind = job.kind().map_err(ApiError)?;
    if kind != JobKind::Mirror {
        return Err(ApiError("只有目录镜像任务支持预览".into()));
    }
    let opts: MirrorOptions = serde_json::from_str(&job.options_json).map_err(|e| ApiError(format!("任务参数损坏: {e}")))?;
    let preview = mirror::preview(&state.store, id, &job.remote_dir, &opts)
        .await
        .map_err(|e| ApiError(format!("{e:?}")))?;
    Ok(Json(json!(preview)))
}

// ---------- 运行记录 ----------

#[derive(Deserialize)]
pub struct RunsQuery {
    #[serde(default)]
    job_id: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

pub async fn list_runs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RunsQuery>,
) -> ApiResult<Json<Value>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let runs = state.store.list_runs(q.job_id, limit)?;
    let jobs = state.store.list_jobs()?;
    let items: Vec<Value> = runs
        .iter()
        .map(|r| {
            let name = jobs.iter().find(|j| j.id == r.job_id).map(|j| j.name.clone());
            run_view(r, name.as_deref())
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

pub async fn get_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let run = state
        .store
        .run_by_id(id)?
        .ok_or_else(|| ApiError("运行记录不存在".into()))?;
    let job = state.store.get_job(run.job_id)?;
    Ok(Json(run_view(&run, job.as_ref().map(|j| j.name.as_str()))))
}

#[derive(Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    offset: u64,
}

pub async fn run_log(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<LogQuery>,
) -> ApiResult<Json<Value>> {
    let run = state
        .store
        .run_by_id(id)?
        .ok_or_else(|| ApiError("运行记录不存在".into()))?;
    let Some(path) = run.log_path else {
        return Ok(Json(json!({ "text": "", "offset": 0, "eof": true })));
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => {
            return Ok(Json(json!({ "text": "", "offset": 0, "eof": true })));
        }
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let mut offset = q.offset.min(len);
    if offset > len {
        offset = 0;
    }
    use tokio::io::AsyncSeekExt;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| ApiError(e.to_string()))?;
    let mut buf = Vec::new();
    // 每次最多读 128 KiB，避免大日志把页面卡住
    (&mut file)
        .take(128 * 1024)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| ApiError(e.to_string()))?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let new_offset = offset + buf.len() as u64;
    Ok(Json(json!({
        "text": text,
        "offset": new_offset,
        "eof": new_offset >= len,
    })))
}

// ---------- 设置 ----------

pub async fn get_policy(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(state.store.policy())))
}

pub async fn set_policy(
    State(state): State<Arc<AppState>>,
    Json(policy): Json<Policy>,
) -> ApiResult<Json<Value>> {
    if policy.poll_ms < 1000 {
        return Err(ApiError("轮询间隔最小 1 秒（再小没有意义还会增加负担）".into()));
    }
    if policy.max_failures == 0 {
        return Err(ApiError("连续失败阈值至少 1".into()));
    }
    state.store.set_policy(&policy)?;
    Ok(Json(json!({ "ok": true })))
}

/// 生成 age 密钥对：私钥只在这里返回一次，不落库。
pub async fn age_keygen() -> ApiResult<Json<Value>> {
    let output = tokio::process::Command::new("age-keygen")
        .output()
        .await
        .map_err(|e| ApiError(format!("运行 age-keygen 失败（容器里可能没装 age）: {e}")))?;
    if !output.status.success() {
        return Err(ApiError(format!(
            "age-keygen 失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let identity = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let recipient = String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|l| l.split("Public key:").nth(1).map(|s| s.trim().to_string()))
        .ok_or_else(|| ApiError("没能解析出 age 公钥".into()))?;
    Ok(Json(json!({
        "identity": identity,
        "recipient": recipient,
        "hint": "私钥只显示这一次，请立刻保存到安全的地方；容器里不保存私钥",
    })))
}

// ---------- 本地目录浏览 ----------

#[derive(Deserialize)]
pub struct BrowseQuery {
    #[serde(default)]
    path: Option<String>,
}

pub async fn browse_local(Query(q): Query<BrowseQuery>) -> ApiResult<Json<Value>> {
    // 默认从宿主机的只读挂载点开始；没有这个挂载时依次退回旧路径和根目录
    let fallback = ["/host", "/mnt/source", "/"]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_dir())
        .unwrap_or("/");
    let root = q
        .path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback);
    let root = std::path::PathBuf::from(root);
    if !root.is_dir() {
        return Err(ApiError(format!("目录不存在: {}", root.display())));
    }
    let mut dirs = Vec::new();
    let mut files = 0u64;
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => dirs.push(json!({
                    "name": entry.file_name().to_string_lossy().to_string(),
                    "path": path.to_string_lossy().to_string(),
                })),
                Ok(t) if t.is_file() => files += 1,
                _ => {}
            }
        }
    }
    dirs.sort_by_key(|v| v["name"].as_str().unwrap_or("").to_string());
    Ok(Json(json!({
        "path": root.to_string_lossy(),
        "parent": root.parent().map(|p| p.to_string_lossy().to_string()),
        "dirs": dirs,
        "file_count": files,
    })))
}
