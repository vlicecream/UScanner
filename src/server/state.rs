use anyhow::Result;
use lru::LruCache;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::db;
use crate::types::{ConfigCache, PhaseInfo, Progress, ProgressPlan, ProgressReporter};

const COMPLETION_CACHE_CAPACITY: usize = 50_000;
const PRIMARY_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const READ_ONLY_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const CACHE_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

// -----------------------------------------------------------------------------
// Progress reporter
// -----------------------------------------------------------------------------

/// Reports refresh progress through the RPC channel.
/// 通过 RPC 通道上报 refresh 进度。
pub struct RpcProgressReporter {
    pub tx: mpsc::Sender<Vec<u8>>,
}

impl ProgressReporter for RpcProgressReporter {
    /// Send one progress event.
    /// 发送一个进度事件。
    fn report(&self, stage: &str, current: usize, total: usize, message: &str) {
        let progress = Progress {
            msg_type: "progress".to_string(),
            stage: stage.to_string(),
            current,
            total,
            message: message.to_string(),
        };

        let _ = send_msgpack_notification(&self.tx, "progress", progress);
    }

    /// Send the refresh phase plan.
    /// 发送 refresh 阶段计划。
    fn report_plan(&self, phases: &[PhaseInfo]) {
        let plan = ProgressPlan {
            msg_type: "progress_plan".to_string(),
            phases: phases.to_vec(),
        };

        let _ = send_msgpack_notification(&self.tx, "progress_plan", plan);
    }
}

/// Encode and send a MessagePack RPC notification.
/// 编码并发送一个 MessagePack RPC notification。
fn send_msgpack_notification<T>(tx: &mpsc::Sender<Vec<u8>>, method: &str, payload: T) -> Result<()>
where
    T: Serialize,
{
    let notification = (2, method, payload);
    let encoded = rmp_serde::to_vec(&notification)?;

    let mut framed = Vec::with_capacity(encoded.len() + 4);
    framed.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
    framed.extend_from_slice(&encoded);

    let _ = tx.blocking_send(framed);
    Ok(())
}

// -----------------------------------------------------------------------------
// Project and asset state
// -----------------------------------------------------------------------------

/// Per-project persistent context stored in registry.
/// 单个工程的持久化上下文，会写入 registry。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    pub db_path: String,

    #[serde(default)]
    pub cache_db_path: Option<String>,

    #[serde(default)]
    pub vcs_hash: Option<String>,

    #[serde(skip, default = "Instant::now")]
    pub last_refresh_at: Instant,
}

/// In-memory asset relationship graph.
/// 内存里的 Unreal 资产关系图。
#[derive(Debug, Clone, Default)]
pub struct AssetGraph {
    /// imported asset/class -> assets that reference it.
    /// 被引用资产/类 -> 引用它的资产集合。
    pub references: HashMap<Arc<str>, HashSet<Arc<str>>>,

    /// parent class -> derived Blueprint/assets.
    /// 父类 -> 派生蓝图/资产集合。
    pub derived: HashMap<Arc<str>, HashSet<Arc<str>>>,

    /// function name -> assets that mention/call it.
    /// 函数名 -> 出现或调用它的资产集合。
    pub functions: HashMap<Arc<str>, HashSet<Arc<str>>>,
}

// -----------------------------------------------------------------------------
// Completion cache
// -----------------------------------------------------------------------------

/// In-memory completion cache keyed by (class_name, prefix).
/// 内存补全缓存，key 是 (class_name, prefix)。
pub struct CompletionCache {
    lru: LruCache<(String, String), CompletionCacheEntry>,
    class_to_keys: HashMap<String, HashSet<(String, String)>>,
}

struct CompletionCacheEntry {
    value: serde_json::Value,
    hit_count: u64,
}

impl CompletionCache {
    /// Create an empty completion cache.
    /// 创建一个空补全缓存。
    pub fn new() -> Self {
        Self {
            lru: LruCache::new(NonZeroUsize::new(COMPLETION_CACHE_CAPACITY).unwrap()),
            class_to_keys: HashMap::new(),
        }
    }

    /// Get a cached completion result.
    /// 获取缓存的补全结果。
    pub fn get(&mut self, class_name: &str, prefix: &str) -> Option<serde_json::Value> {
        let key = make_completion_key(class_name, prefix);

        let entry = self.lru.get_mut(&key)?;
        entry.hit_count += 1;

        Some(entry.value.clone())
    }

    /// Insert or update a cached completion result.
    /// 插入或更新补全缓存结果。
    pub fn put(&mut self, class_name: &str, prefix: &str, value: serde_json::Value) {
        let key = make_completion_key(class_name, prefix);

        self.class_to_keys
            .entry(class_name.to_string())
            .or_default()
            .insert(key.clone());

        self.lru.put(
            key,
            CompletionCacheEntry {
                value,
                hit_count: 1,
            },
        );
    }

    /// Invalidate all completion entries for one class.
    /// 清理某个 class 对应的所有补全缓存。
    pub fn invalidate_class(&mut self, class_name: &str) {
        let Some(keys) = self.class_to_keys.remove(class_name) else {
            return;
        };

        info!(
            "Invalidating completion cache for class: {} ({} entries)",
            class_name,
            keys.len()
        );

        for key in keys {
            self.lru.pop(&key);
        }
    }

    /// Clear all completion cache entries.
    /// 清空全部补全缓存。
    pub fn clear(&mut self) {
        self.lru.clear();
        self.class_to_keys.clear();
    }

    /// Return current cache entry count.
    /// 返回当前缓存条目数量。
    pub fn len(&self) -> usize {
        self.lru.len()
    }

    /// Return true if cache is empty.
    /// 判断缓存是否为空。
    pub fn is_empty(&self) -> bool {
        self.lru.is_empty()
    }
}

/// Build normalized completion cache key.
/// 构造规范化补全缓存 key。
fn make_completion_key(class_name: &str, prefix: &str) -> (String, String) {
    (class_name.trim().to_string(), prefix.trim().to_string())
}

// -----------------------------------------------------------------------------
// AppState
// -----------------------------------------------------------------------------

/// Shared server state.
/// server 全局共享状态。
pub struct AppState {
    pub projects: Mutex<HashMap<String, ProjectContext>>,
    pub connections: Mutex<HashMap<String, Arc<Mutex<rusqlite::Connection>>>>,
    pub persistent_cache_connections: Mutex<HashMap<String, Arc<Mutex<rusqlite::Connection>>>>,
    pub active_refreshes: Mutex<HashSet<String>>,
    pub active_asset_scans: Mutex<HashSet<String>>,
    pub watcher: Mutex<notify::RecommendedWatcher>,
    pub registry_path: Option<PathBuf>,
    pub active_clients: Mutex<HashSet<u32>>,
    pub last_activity: Mutex<Instant>,
    pub asset_graphs: Mutex<HashMap<String, AssetGraph>>,
    pub config_caches: Mutex<HashMap<String, ConfigCache>>,
    pub completion_caches: Mutex<HashMap<String, Arc<Mutex<CompletionCache>>>>,
}

impl AppState {
    /// Save registered projects to registry JSON file.
    /// 把已注册工程写入 registry JSON 文件。
    pub fn save_registry(&self) -> Result<()> {
        let Some(path) = &self.registry_path else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let projects = self.projects.lock();
        let json = serde_json::to_string_pretty(&*projects)?;

        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load registered projects from registry JSON file.
    /// 从 registry JSON 文件加载已注册工程。
    pub fn load_registry(path: &Path) -> HashMap<String, ProjectContext> {
        let data = match std::fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => return HashMap::new(),
        };

        match serde_json::from_str(&data) {
            Ok(projects) => projects,
            Err(err) => {
                warn!("Failed to parse project registry {}: {}", path.display(), err);
                HashMap::new()
            }
        }
    }

    /// Register one active Neovim client.
    /// 注册一个活跃 Neovim 客户端。
    pub fn register_client(&self, pid: u32) {
        let inserted = {
            let mut clients = self.active_clients.lock();
            clients.insert(pid)
        };

        if inserted {
            info!("Registered client PID: {}", pid);
        }

        *self.last_activity.lock() = Instant::now();
    }

    /// Get or open the primary read/write database connection.
    /// 获取或打开主读写数据库连接。
    pub fn get_connection(&self, db_path: &str) -> Result<Arc<Mutex<rusqlite::Connection>>> {
        {
            let conns = self.connections.lock();
            if let Some(conn) = conns.get(db_path) {
                return Ok(Arc::clone(conn));
            }
        }

        let conn = open_primary_connection(db_path)?;
        let conn = Arc::new(Mutex::new(conn));

        let mut conns = self.connections.lock();
        let existing = conns
            .entry(db_path.to_string())
            .or_insert_with(|| Arc::clone(&conn));

        Ok(Arc::clone(existing))
    }

    /// Open a new read-only database connection for parallel queries.
    /// 打开新的只读数据库连接，用于并发 query。
    pub fn get_read_only_connection(&self, db_path: &str) -> Result<rusqlite::Connection> {
        open_read_only_connection(db_path)
    }

    /// Get or create the per-project completion cache.
    /// 获取或创建某个工程的补全缓存。
    pub fn get_completion_cache(&self, project_root: &str) -> Arc<Mutex<CompletionCache>> {
        {
            let caches = self.completion_caches.lock();
            if let Some(cache) = caches.get(project_root) {
                return Arc::clone(cache);
            }
        }

        let cache = Arc::new(Mutex::new(CompletionCache::new()));

        let mut caches = self.completion_caches.lock();
        let existing = caches
            .entry(project_root.to_string())
            .or_insert_with(|| Arc::clone(&cache));

        Arc::clone(existing)
    }

    /// Get or open persistent completion cache database.
    /// 获取或打开持久化补全缓存数据库。
    pub fn get_persistent_cache_connection(
        &self,
        cache_db_path: &str,
    ) -> Result<Arc<Mutex<rusqlite::Connection>>> {
        {
            let conns = self.persistent_cache_connections.lock();
            if let Some(conn) = conns.get(cache_db_path) {
                return Ok(Arc::clone(conn));
            }
        }

        let conn = open_persistent_cache_connection(cache_db_path)?;
        let conn = Arc::new(Mutex::new(conn));

        let mut conns = self.persistent_cache_connections.lock();
        let existing = conns
            .entry(cache_db_path.to_string())
            .or_insert_with(|| Arc::clone(&conn));

        Ok(Arc::clone(existing))
    }

    /// Drop cached DB connections for one project.
    /// 删除某个工程相关的缓存 DB 连接。
    pub fn drop_connections(&self, db_path: &str, cache_db_path: Option<&str>) {
        self.connections.lock().remove(db_path);

        if let Some(cache_path) = cache_db_path {
            self.persistent_cache_connections.lock().remove(cache_path);
        }
    }
}

// -----------------------------------------------------------------------------
// SQLite connection helpers
// -----------------------------------------------------------------------------

/// Open primary read/write SQLite connection.
/// 打开主读写 SQLite 连接。
fn open_primary_connection(db_path: &str) -> Result<rusqlite::Connection> {
    info!("Opening primary database connection: {}", db_path);

    let _ = db::ensure_correct_version(db_path)?;

    let conn = rusqlite::Connection::open(db_path)?;
    conn.busy_timeout(PRIMARY_DB_BUSY_TIMEOUT)?;

    configure_primary_connection(&conn)?;

    Ok(conn)
}

/// Open read-only SQLite connection for queries.
/// 打开只读 SQLite 连接，用于查询。
fn open_read_only_connection(db_path: &str) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    conn.busy_timeout(READ_ONLY_DB_BUSY_TIMEOUT)?;
    configure_read_only_connection(&conn)?;

    Ok(conn)
}

/// Open persistent cache SQLite connection.
/// 打开持久化缓存 SQLite 连接。
fn open_persistent_cache_connection(cache_db_path: &str) -> Result<rusqlite::Connection> {
    info!("Opening persistent cache database: {}", cache_db_path);

    let conn = rusqlite::Connection::open(cache_db_path)?;
    conn.busy_timeout(CACHE_DB_BUSY_TIMEOUT)?;

    configure_cache_connection(&conn)?;
    db::init_cache_db(&conn)?;

    Ok(conn)
}

/// Configure primary read/write connection.
/// 配置主读写连接。
fn configure_primary_connection(conn: &rusqlite::Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "cache_size", "-500000")?;
    conn.pragma_update(None, "mmap_size", "1073741824")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

/// Configure read-only query connection.
/// 配置只读 query 连接。
fn configure_read_only_connection(conn: &rusqlite::Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "cache_size", "-4000")?;
    conn.pragma_update(None, "mmap_size", "0")?;
    conn.pragma_update(None, "temp_store", "FILE")?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(())
}

/// Configure persistent cache connection.
/// 配置持久化缓存连接。
fn configure_cache_connection(conn: &rusqlite::Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "OFF")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}
