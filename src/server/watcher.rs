use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, error, info};

use crate::server::state::AppState;
use crate::server::utils::{normalize_to_native, normalize_path_key};
use crate::{db, scanner};

const SOURCE_EXTENSIONS: &[&str] = &["h", "hh", "hpp", "cpp", "cc", "cxx", "inl", "cs"];
const ASSET_EXTENSIONS: &[&str] = &["uasset", "umap"];

/// Handle one filesystem change event.
/// 处理一个文件系统变化事件。
pub async fn handle_file_change(state: Arc<AppState>, path: PathBuf) {
    if !path.exists() || !path.is_file() {
        return;
    }

    let normalized_path = normalize_watched_path(&path);
    let Some(project) = find_project_for_path(&state, &normalized_path) else {
        return;
    };

    let ext = file_extension(&path);

    if ext == "ini" {
        mark_config_cache_dirty(&state, &project.root_key);
        return;
    }

    if ASSET_EXTENSIONS.contains(&ext.as_str()) {
        handle_asset_change(state, project.root_key, path, &ext).await;
        return;
    }

    if SOURCE_EXTENSIONS.contains(&ext.as_str()) {
        handle_source_change(state, project, normalized_path).await;
    }
}

// -----------------------------------------------------------------------------
// Project matching
// -----------------------------------------------------------------------------

/// Matched project info for one changed file.
/// 某个变更文件匹配到的工程信息。
struct MatchedProject {
    root_key: String,
    db_path_unix: String,
}

/// Find the registered project that owns the changed path.
/// 查找这个变更文件属于哪个已注册工程。
fn find_project_for_path(state: &AppState, normalized_path: &str) -> Option<MatchedProject> {
    let normalized_lower = normalized_path.to_ascii_lowercase();

    let projects = state.projects.lock();

    projects
        .iter()
        .filter_map(|(root, ctx)| {
            let root_lower = normalize_path_key(root).to_ascii_lowercase();

            if normalized_lower.starts_with(&root_lower) {
                Some(MatchedProject {
                    root_key: root.clone(),
                    db_path_unix: ctx.db_path.clone(),
                })
            } else {
                None
            }
        })
        .max_by_key(|project| project.root_key.len())
}

// -----------------------------------------------------------------------------
// Config changes
// -----------------------------------------------------------------------------

/// Mark config cache dirty when an .ini file changes.
/// 当 .ini 文件变化时，标记配置缓存失效。
fn mark_config_cache_dirty(state: &AppState, root_key: &str) {
    let mut caches = state.config_caches.lock();

    if let Some(cache) = caches.get_mut(root_key) {
        cache.is_dirty = true;
        info!("Config cache marked dirty: {}", root_key);
    }
}

// -----------------------------------------------------------------------------
// Asset changes
// -----------------------------------------------------------------------------

/// Handle .uasset or .umap change.
/// 处理 .uasset 或 .umap 资产变化。
async fn handle_asset_change(state: Arc<AppState>, root_key: String, path: PathBuf, ext: &str) {
    if !is_important_asset(&path, ext) {
        return;
    }

    crate::server::asset::update_single_asset(state, &root_key, &path).await;
}

/// Return true if the asset is useful for navigation/search indexes.
/// 判断资产是否值得进入导航/搜索索引。
fn is_important_asset(path: &Path, ext: &str) -> bool {
    if ext == "umap" {
        return true;
    }

    let filename = path.file_name().and_then(|name| name.to_str()).unwrap_or("");

    filename.starts_with("BP_")
        || filename.starts_with("ABP_")
        || filename.starts_with("WBP_")
        || filename.starts_with("AM_")
        || filename.starts_with("DA_")
        || filename.starts_with("DT_")
}

// -----------------------------------------------------------------------------
// Source changes
// -----------------------------------------------------------------------------

/// Handle C++/C#/header source change.
/// 处理 C++/C#/头文件源码变化。
async fn handle_source_change(
    state: Arc<AppState>,
    project: MatchedProject,
    path_str_unix: String,
) {
    let db_path_native = normalize_to_native(&project.db_path_unix);

    let conn = match state.get_connection(&db_path_native) {
        Ok(conn) => conn,
        Err(err) => {
            error!("Watcher failed to open DB connection: {}", err);
            return;
        }
    };

    tokio::task::spawn_blocking(move || {
        let mut conn = conn.lock();

        let module_id = match db::get_module_id_for_path(&conn, &path_str_unix) {
            Ok(Some(module_id)) => module_id,
            Ok(None) => {
                debug!("Changed source file has no module: {}", path_str_unix);
                return;
            }
            Err(err) => {
                error!("Failed to resolve module for {}: {}", path_str_unix, err);
                return;
            }
        };

        info!("File change detected, rescanning: {}", path_str_unix);

        let language = tree_sitter_unreal_cpp::LANGUAGE.into();

        let query = match tree_sitter::Query::new(&language, scanner::QUERY_STR) {
            Ok(query) => query,
            Err(err) => {
                error!("Failed to compile scanner query: {}", err);
                return;
            }
        };

        let include_query = match tree_sitter::Query::new(&language, scanner::INCLUDE_QUERY_STR) {
            Ok(query) => query,
            Err(err) => {
                error!("Failed to compile include query: {}", err);
                return;
            }
        };

        let input = crate::types::InputFile {
            path: path_str_unix.clone(),
            mtime: file_mtime_seconds(&path_str_unix),
            old_hash: None,
            module_id: Some(module_id),
            db_path: Some(db_path_native),
        };

        let parse_result = match scanner::process_file(&input, &language, &query, &include_query) {
            Ok(result) => result,
            Err(err) => {
                error!("Failed to scan changed file {}: {}", path_str_unix, err);
                return;
            }
        };

        let classes_to_invalidate = parse_result
            .data
            .as_ref()
            .map(|data| {
                data.classes
                    .iter()
                    .map(|class| class.class_name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if let Err(err) = db::save_to_db(
            &mut conn,
            &[parse_result],
            Arc::new(crate::types::StdoutReporter),
        ) {
            error!("Watcher failed to save scan result: {}", err);
            return;
        }

        let cache = state.get_completion_cache(&project.root_key);
        let mut cache = cache.lock();

        for class_name in classes_to_invalidate {
            cache.invalidate_class(&class_name);
        }
    });
}

// -----------------------------------------------------------------------------
// Path helpers
// -----------------------------------------------------------------------------

/// Normalize a watched filesystem path into slash-separated project path.
/// 把 watcher 收到的文件路径规范化成斜杠分隔路径。
fn normalize_watched_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let without_unc_prefix = raw
        .strip_prefix(r"\\?\")
        .or_else(|| raw.strip_prefix("//?/"))
        .unwrap_or(&raw);

    let mut normalized = without_unc_prefix.replace('\\', "/");

    if cfg!(target_os = "windows") && normalized.len() >= 2 && normalized.as_bytes()[1] == b':' {
        normalized.replace_range(0..1, &normalized[0..1].to_ascii_uppercase());
    }

    normalized
}

/// Get lowercase file extension without dot.
/// 获取小写扩展名，不包含点号。
fn file_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Get file modified time in Unix seconds.
/// 获取文件修改时间，单位是 Unix 秒。
fn file_mtime_seconds(path: &str) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
