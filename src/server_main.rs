use anyhow::Result;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{error, info};
use u_scanner::server::handle_connection;
use u_scanner::server::state::AppState;
use u_scanner::server::utils::normalize_path_key;
use u_scanner::server::watcher::handle_file_change;

const DEFAULT_PORT: u16 = 30110;
const WATCH_EVENT_CHANNEL_SIZE: usize = 256;
const WATCH_DEBOUNCE_MS: u64 = 50;
const CLIENT_CHECK_INTERVAL_SECS: u64 = 60;
const IDLE_SHUTDOWN_SECS: u64 = 600;

/// Main entry for the UCore/UNL backend server.
/// UCore/UNL 后端 server 的主入口。
#[tokio::main]
async fn main() -> Result<()> {
    let startup = StartupArgs::from_env();

    init_logging(startup.registry_path.as_ref())?;
    install_panic_hook();

    info!("--- UCore Server Starting (MsgPack TCP) ---");

    let (watch_tx, watch_rx) = mpsc::channel::<PathBuf>(WATCH_EVENT_CHANNEL_SIZE);
    let watcher = create_file_watcher(watch_tx)?;

    let state = Arc::new(AppState {
        projects: Mutex::new(load_initial_projects(startup.registry_path.as_ref())),
        connections: Mutex::new(HashMap::new()),
        persistent_cache_connections: Mutex::new(HashMap::new()),
        active_refreshes: Mutex::new(HashSet::new()),
        active_asset_scans: Mutex::new(HashSet::new()),
        watcher: Mutex::new(watcher),
        registry_path: startup.registry_path.clone(),
        active_clients: Mutex::new(HashSet::new()),
        last_activity: Mutex::new(Instant::now()),
        asset_graphs: Mutex::new(HashMap::new()),
        config_caches: Mutex::new(HashMap::new()),
        completion_caches: Mutex::new(HashMap::new()),
    });

    spawn_watch_event_loop(state.clone(), watch_rx);
    spawn_client_lifecycle_loop(state.clone());

    run_tcp_server(startup.port, state).await
}

// -----------------------------------------------------------------------------
// Startup args
// -----------------------------------------------------------------------------

/// Parsed command line arguments.
/// 解析后的命令行参数。
struct StartupArgs {
    port: u16,
    registry_path: Option<PathBuf>,
}

impl StartupArgs {
    /// Parse args: `<port> <registry_path>`.
    /// 解析参数：`<端口> <registry路径>`。
    fn from_env() -> Self {
        let args = std::env::args().collect::<Vec<_>>();

        let port = args
            .get(1)
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);

        let registry_path = args.get(2).map(PathBuf::from);

        Self {
            port,
            registry_path,
        }
    }
}

// -----------------------------------------------------------------------------
// Logging
// -----------------------------------------------------------------------------

/// Initialize file logging.
/// 初始化文件日志。
fn init_logging(registry_path: Option<&PathBuf>) -> Result<()> {
    let log_path = if let Some(path) = registry_path {
        path.parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("u_scanner.log")
    } else {
        PathBuf::from("u_scanner.log")
    };

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(log_file)
        .init();

    info!("Logging to {}", log_path.display());

    Ok(())
}

/// Install panic hook so panics are written to log file.
/// 安装 panic hook，把 panic 写入日志。
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        error!("PANIC: {}", panic_info);
    }));
}

// -----------------------------------------------------------------------------
// App state
// -----------------------------------------------------------------------------

/// Load project registry and normalize project keys.
/// 加载工程 registry，并规范化工程 key。
fn load_initial_projects(
    registry_path: Option<&PathBuf>,
) -> HashMap<String, u_scanner::server::state::ProjectContext> {
    let Some(path) = registry_path else {
        return HashMap::new();
    };

    AppState::load_registry(path)
        .into_iter()
        .map(|(root, ctx)| (normalize_path_key(&root), ctx))
        .collect()
}

// -----------------------------------------------------------------------------
// File watcher
// -----------------------------------------------------------------------------

/// Create notify watcher and forward paths into async channel.
/// 创建 notify watcher，并把变更路径转发到异步 channel。
fn create_file_watcher(
    tx: mpsc::Sender<PathBuf>,
) -> Result<notify::RecommendedWatcher> {
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };

        for path in event.paths {
            let _ = tx.blocking_send(path);
        }
    })?;

    Ok(watcher)
}

/// Spawn async loop that debounces filesystem events.
/// 启动异步循环，对文件系统事件做 debounce。
fn spawn_watch_event_loop(state: Arc<AppState>, mut rx: mpsc::Receiver<PathBuf>) {
    tokio::spawn(async move {
        let mut last_seen = HashMap::<PathBuf, Instant>::new();

        while let Some(path) = rx.recv().await {
            if should_skip_by_debounce(&mut last_seen, &path) {
                continue;
            }

            handle_file_change(state.clone(), path).await;
        }
    });
}

/// Return true if this event is too close to the previous one.
/// 如果事件距离上次太近，则跳过。
fn should_skip_by_debounce(
    last_seen: &mut HashMap<PathBuf, Instant>,
    path: &PathBuf,
) -> bool {
    if let Some(last) = last_seen.get(path) {
        if last.elapsed() < Duration::from_millis(WATCH_DEBOUNCE_MS) {
            return true;
        }
    }

    last_seen.insert(path.clone(), Instant::now());
    false
}

// -----------------------------------------------------------------------------
// Client lifecycle
// -----------------------------------------------------------------------------

/// Spawn loop that removes dead clients and exits when idle.
/// 启动客户端生命周期循环：清理已退出客户端，并在空闲时退出。
fn spawn_client_lifecycle_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut system = System::new_all();

        loop {
            tokio::time::sleep(Duration::from_secs(CLIENT_CHECK_INTERVAL_SECS)).await;

            system.refresh_processes(ProcessesToUpdate::All, true);

            remove_dead_clients(&state, &system);

            if should_shutdown_for_idle(&state) {
                info!(
                    "No active clients for {}s. Shutting down server...",
                    IDLE_SHUTDOWN_SECS
                );
                std::process::exit(0);
            }
        }
    });
}

/// Remove client PIDs that no longer exist.
/// 删除已经不存在的客户端 PID。
fn remove_dead_clients(state: &AppState, system: &System) {
    let mut clients = state.active_clients.lock();

    let dead = clients
        .iter()
        .copied()
        .filter(|pid| system.process(Pid::from(*pid as usize)).is_none())
        .collect::<Vec<_>>();

    for pid in dead {
        clients.remove(&pid);
        info!("Client process disconnected: {}", pid);
    }
}

/// Return true if server should stop due to no active clients.
/// 判断 server 是否因为没有活跃客户端而退出。
fn should_shutdown_for_idle(state: &AppState) -> bool {
    let clients = state.active_clients.lock();

    if clients.is_empty() {
        let last_activity = *state.last_activity.lock();
        return last_activity.elapsed() > Duration::from_secs(IDLE_SHUTDOWN_SECS);
    }

    drop(clients);

    *state.last_activity.lock() = Instant::now();
    false
}

// -----------------------------------------------------------------------------
// TCP server
// -----------------------------------------------------------------------------

/// Bind TCP listener and serve clients forever.
/// 绑定 TCP 端口并持续服务客户端。
async fn run_tcp_server(port: u16, state: Arc<AppState>) -> Result<()> {
    let address = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&address).await?;

    info!("UCore server listening on {}", address);

    loop {
        let (socket, peer) = listener.accept().await?;

        info!("Accepted client: {}", peer);

        let state = state.clone();

        tokio::spawn(async move {
            handle_connection(socket, state).await;
        });
    }
}
