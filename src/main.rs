// 备份管理器：网页配置、定时执行，把本地数据经 CD2 上传到 115。
//
// 重要约束：程序只调用 CD2 的 gRPC 接口，不直接访问 115 的任何接口。
// 所有对 115 的读写都由 CD2 完成。

mod cd2;
mod config;
mod destination;
mod jobs;
mod model;
mod runner;
mod scheduler;
mod secrets;
mod store;
mod web;

// CD2 的 gRPC 接口，由 build.rs 用官方 proto 生成。
pub mod pb {
    tonic::include_proto!("clouddrive");
}

use std::sync::Arc;
use std::time::Duration;

use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::secrets::Sealer;
use crate::store::Store;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let cfg = Config::from_env();

    // 运维子命令：只读探测 CD2（不需要令牌，不写任何东西）
    if args.len() >= 3 && args[1] == "--cd2-check" {
        return cd2_check(&args[2]).await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cfg.log_level))
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    std::fs::create_dir_all(&cfg.data_dir)?;
    std::fs::create_dir_all(cfg.log_dir())?;

    let store = Arc::new(Store::open(&cfg.db_path())?);

    // 运维子命令：重置管理员密码（忘记密码时用，容器里执行）
    if args.len() >= 3 && args[1] == "--set-password" {
        let password = &args[2];
        let hash = web::hash_password(password)?;
        store.set_setting("admin_password_hash", &hash)?;
        store.set_setting("setup_code", "")?;
        println!("管理员密码已更新。");
        if password.chars().count() < 8 {
            eprintln!("注意：密码短于 8 位，只适合可信内网使用。");
        }
        return Ok(());
    }

    match store.fail_stale_runs() {
        Ok(0) => {}
        Ok(n) => tracing::warn!("清理了 {n} 条上次被中断的运行记录"),
        Err(e) => tracing::error!("清理中断记录失败: {e}"),
    }

    let sealer = Arc::new(Sealer::load_or_create(&cfg.key_path())?);
    let setup_code = ensure_admin(&store, &cfg)?;
    sweep_old_logs(&cfg);
    match store.prune_sessions(store::now() - web::SESSION_IDLE_SECS) {
        Ok(0) => {}
        Ok(n) => tracing::info!("清理了 {n} 个过期登录会话"),
        Err(e) => tracing::warn!("清理过期会话失败: {e}"),
    }

    let runner = runner::Runner::start(store.clone(), sealer.clone(), cfg.clone());
    scheduler::spawn(runner.clone(), store.clone());

    let state = Arc::new(web::AppState {
        store,
        sealer,
        runner,
        config: cfg.clone(),
        setup_code,
    });

    let app = web::router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(
        "备份管理器已启动：http://{}（数据目录 {}）",
        cfg.bind,
        cfg.data_dir.display()
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("已退出");
    Ok(())
}

/// 只读探测 CD2 是否可达、协议是否对得上（GetSystemInfo 是公开接口）。
async fn cd2_check(address: &str) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("quantumark=info"))
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    let policy = model::Policy::default();
    let client = cd2::Cd2Client::new(
        address,
        model::DestSecret::Token { token: String::new() },
        &policy,
    )?;
    let sys = client.system_info().await?;
    println!("CD2 可连通：{address}");
    println!(
        "  已登录: {}  用户: {}  就绪: {}",
        sys.is_login, sys.user_name, sys.system_ready
    );
    match client.runtime_info().await {
        Ok(rt) => println!(
            "  产品: {} {}（CloudAPI {}，系统 {}）",
            rt.product_name, rt.product_version, rt.cloud_api_version, rt.os_info
        ),
        Err(e) => println!("  版本查询需要令牌权限: {e}"),
    }
    Ok(())
}

/// 初始化管理员密码：
///   - 已设置过 → 什么都不做
///   - 配了 QM_ADMIN_PASSWORD → 用它
///   - 否则生成一次性设置码打到日志里，首次进网页时输入
fn ensure_admin(store: &Store, cfg: &Config) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let existing = store.get_setting("admin_password_hash")?.unwrap_or_default();
    if !existing.trim().is_empty() {
        return Ok(None);
    }
    if let Some(password) = &cfg.admin_password {
        let hash = web::hash_password(password)?;
        store.set_setting("admin_password_hash", &hash)?;
        tracing::info!("已用 QM_ADMIN_PASSWORD 初始化管理员密码");
        return Ok(None);
    }
    // 设置码落库：容器重启后仍然是同一个，设置完密码后清掉
    let code = match store.get_setting("setup_code")? {
        Some(existing) if !existing.trim().is_empty() => existing,
        _ => {
            let code = random_code(8);
            store.set_setting("setup_code", &code)?;
            code
        }
    };
    tracing::warn!("首次启动：请打开网页，用初始设置码 {code} 设置管理员密码（只需一次）");
    Ok(Some(code))
}

fn random_code(len: usize) -> String {
    // 去掉容易看错的 0/O/1/I
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut buf = vec![0u8; len];
    rand::fill(&mut buf[..]);
    buf.iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

/// 清理 30 天前的运行日志。
fn sweep_old_logs(cfg: &Config) {
    let Ok(entries) = std::fs::read_dir(cfg.log_dir()) else {
        return;
    };
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
    let mut removed = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("log") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else { continue };
        if modified < cutoff && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!("清理了 {removed} 个 30 天前的运行日志");
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
