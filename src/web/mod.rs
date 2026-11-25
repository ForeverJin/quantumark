// 网页服务：axum 路由 + 会话鉴权 + 内嵌单页界面（无前端构建步骤）。

pub mod api;
mod auth;

pub use auth::hash_password;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};

use crate::config::Config;
use crate::runner::Runner;
use crate::secrets::Sealer;
use crate::store::Store;

/// 会话最长闲置时间（秒）。
pub const SESSION_IDLE_SECS: i64 = 24 * 3600;

/// 应用状态。
pub struct AppState {
    pub store: Arc<Store>,
    pub sealer: Arc<Sealer>,
    pub runner: Runner,
    pub config: Config,
    /// 首次启动的设置码（没配置管理员密码时才有）
    pub setup_code: Option<String>,
}

impl AppState {
    /// 是否需要先设置管理员密码。
    pub fn needs_setup(&self) -> bool {
        self.store
            .get_setting("admin_password_hash")
            .ok()
            .flatten()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/api/overview", get(api::overview))
        .route("/api/password", post(api::change_password))
        // 目的地
        .route("/api/destinations", get(api::list_destinations).post(api::create_destination))
        .route(
            "/api/destinations/probe",
            post(api::probe_unsaved_destination),
        )
        .route(
            "/api/destinations/{id}",
            put(api::update_destination).delete(api::delete_destination),
        )
        .route("/api/destinations/{id}/probe", post(api::probe_destination))
        .route("/api/destinations/{id}/selftest", post(api::selftest_destination))
        .route("/api/destinations/{id}/list", get(api::list_remote_dir))
        .route("/api/destinations/{id}/delete-path", post(api::delete_remote_path))
        .route("/api/token/create", post(api::create_cd2_token))
        // 任务
        .route("/api/jobs", get(api::list_jobs).post(api::create_job))
        .route(
            "/api/jobs/{id}",
            put(api::update_job).delete(api::delete_job),
        )
        .route("/api/jobs/{id}/run", post(api::run_job))
        .route("/api/jobs/{id}/stop", post(api::stop_job))
        .route("/api/jobs/{id}/dry-run", post(api::dry_run_job))
        .route("/api/jobs/{id}/reset-index", post(api::reset_index))
        // 运行记录
        .route("/api/runs", get(api::list_runs))
        .route("/api/runs/{id}", get(api::get_run))
        .route("/api/runs/{id}/log", get(api::run_log))
        // 设置与工具
        .route("/api/policy", get(api::get_policy).put(api::set_policy))
        .route("/api/age/keygen", post(api::age_keygen))
        .route("/api/browse", get(api::browse_local))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/", get(index))
        .route("/static/app.js", get(app_js))
        .route("/static/style.css", get(style_css))
        .route("/health", get(health))
        .route("/api/login", post(api::login))
        .route("/api/logout", post(api::logout))
        .route("/api/me", get(api::me))
        .route("/api/setup", post(api::setup))
        .merge(protected)
        .with_state(state)
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("ui/index.html"),
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        include_str!("ui/app.js"),
    )
}

async fn style_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("ui/style.css"),
    )
}

async fn health() -> impl IntoResponse {
    "ok"
}

/// 受保护路由的中间件：校验会话；写操作额外校验 CSRF 头。
async fn require_auth(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let Some(token) = cookie_value(req.headers(), "qm_session") else {
        return unauthorized();
    };

    let csrf_token = match state.store.get_session(&token) {
        Ok(Some((csrf, last_seen))) => {
            let now = crate::store::now();
            if now - last_seen > SESSION_IDLE_SECS {
                let _ = state.store.drop_session(&token);
                return unauthorized();
            }
            // 最多每分钟写一次 last_seen，避免每个请求都写库
            if now - last_seen >= 60 {
                let _ = state.store.touch_session(&token, now);
            }
            csrf
        }
        Ok(None) => return unauthorized(),
        Err(e) => {
            tracing::error!("读取会话失败: {e}");
            return unauthorized();
        }
    };

    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        let csrf = req
            .headers()
            .get("x-qm-csrf")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if csrf.is_empty() || csrf != csrf_token {
            return (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({"error": "CSRF 校验失败，请刷新页面"}))).into_response();
        }
    }

    next.run(req).await
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({"error": "未登录或会话已过期"})),
    )
        .into_response()
}

/// 从 Cookie 头里取一个值。
pub fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
