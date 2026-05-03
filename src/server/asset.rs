use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::server::state::{AppState, AssetGraph};
use crate::server::utils::normalize_path_key;
use crate::uasset::UAssetParser;

const DISCOVERY_MAX_DEPTH: usize = 4;
const LOG_EVERY: usize = 1000;

/// Run a targeted asset scan for one Unreal project root.
/// 对一个 Unreal 工程根目录执行定向资产扫描。
pub async fn handle_asset_scan(state: Arc<AppState>, project_root: String) {
    let root_key = normalize_path_key(&project_root);
    let _guard = ActiveAssetScanGuard::new(state.clone(), root_key.clone());

    info!("Starting asset scan: {}", project_root);

    let root = PathBuf::from(project_root.clone());
    let scan_result = tokio::task::spawn_blocking(move || scan_project_assets(&root)).await;

    match scan_result {
        Ok(Ok(report)) => {
            info!(
                "Asset scan completed: {} files, {} parsed, {} skipped, {} errors",
                report.total_seen,
                report.parsed.len(),
                report.skipped,
                report.errors
            );

            let graph = build_asset_graph(report.parsed);

            let mut graphs = state.asset_graphs.lock();
            graphs.insert(root_key, graph);
        }

        Ok(Err(err)) => {
            warn!("Asset scan failed for {}: {}", project_root, err);
        }

        Err(join_err) => {
            warn!("Asset scan task failed for {}: {}", project_root, join_err);
        }
    }
}

/// Update one changed asset inside the in-memory graph.
/// 增量更新内存资产图里的单个资产。
pub async fn update_single_asset(state: Arc<AppState>, project_root: &str, file_path: &Path) {
    let root_key = normalize_path_key(project_root);
    let path = file_path.to_path_buf();

    let parse_result = tokio::task::spawn_blocking(move || parse_asset_record(&path)).await;

    match parse_result {
        Ok(Ok(record)) => {
            let mut graphs = state.asset_graphs.lock();

            if let Some(graph) = graphs.get_mut(&root_key) {
                remove_asset_from_graph(graph, &record.asset_path);
                insert_asset_record(graph, record);
                info!("Incremental asset update: {}", file_path.display());
            }
        }

        Ok(Err(err)) => {
            warn!("Failed to update asset {}: {}", file_path.display(), err);
        }

        Err(join_err) => {
            warn!(
                "Incremental asset update task failed for {}: {}",
                file_path.display(),
                join_err
            );
        }
    }
}

// -----------------------------------------------------------------------------
// Scan lifecycle
// -----------------------------------------------------------------------------

/// Guard that clears active_asset_scans when the scan exits.
/// 扫描退出时自动清理 active_asset_scans 标记。
struct ActiveAssetScanGuard {
    state: Arc<AppState>,
    root_key: String,
}

impl ActiveAssetScanGuard {
    /// Create a new guard for one project root.
/// 为某个工程 root 创建扫描保护对象。
    fn new(state: Arc<AppState>, root_key: String) -> Self {
        Self { state, root_key }
    }
}

impl Drop for ActiveAssetScanGuard {
    fn drop(&mut self) {
        let mut active = self.state.active_asset_scans.lock();
        active.remove(&self.root_key);
        info!("Asset scan flag cleared for: {}", self.root_key);
    }
}

/// Full scan report produced by the blocking worker.
/// 阻塞扫描线程返回的完整扫描报告。
struct AssetScanReport {
    total_seen: usize,
    skipped: usize,
    errors: usize,
    parsed: Vec<AssetRecord>,
}

/// Parsed information for one asset file.
/// 单个资产文件解析出来的信息。
#[derive(Debug)]
struct AssetRecord {
    asset_path: String,
    parent_class: Option<String>,
    imports: Vec<String>,
    functions: Vec<String>,
}

/// Scan one Unreal project root and parse selected assets.
/// 扫描一个 Unreal 工程根目录，并解析筛选后的资产。
fn scan_project_assets(project_root: &Path) -> Result<AssetScanReport> {
    let content_dirs = discover_content_dirs(project_root);
    let asset_files = collect_candidate_assets(&content_dirs);

    let total_seen = asset_files.len();

    let parsed_results = asset_files
        .par_iter()
        .enumerate()
        .filter_map(|(index, path)| {
            if index > 0 && index % LOG_EVERY == 0 {
                debug!("Asset scan progress: {} files visited", index);
            }

            match parse_asset_record(path) {
                Ok(record) => Some(Ok(record)),
                Err(err) => Some(Err((path.clone(), err))),
            }
        })
        .collect::<Vec<_>>();

    let mut parsed = Vec::new();
    let mut errors = 0usize;

    for item in parsed_results {
        match item {
            Ok(record) => parsed.push(record),
            Err((path, err)) => {
                errors += 1;
                warn!("Failed to parse asset {}: {}", path.display(), err);
            }
        }
    }

    Ok(AssetScanReport {
        total_seen,
        skipped: 0,
        errors,
        parsed,
    })
}

/// Find Content directories under the project root.
/// 在工程根目录下查找 Content 目录。
fn discover_content_dirs(project_root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();

    let walker = ignore::WalkBuilder::new(project_root)
        .hidden(false)
        .git_ignore(false)
        .follow_links(true)
        .max_depth(Some(DISCOVERY_MAX_DEPTH))
        .filter_entry(|entry| !is_ignored_dir(entry.path()))
        .build();

    for entry in walker.filter_map(|entry| entry.ok()) {
        let path = entry.path();

        if entry.file_type().map_or(false, |ty| ty.is_dir())
            && entry.file_name().to_string_lossy().eq_ignore_ascii_case("Content")
        {
            let normalized = normalize_path(path);
            if seen.insert(normalized) {
                dirs.push(path.to_path_buf());
            }
        }
    }

    dirs
}

/// Collect important .uasset/.umap files from Content directories.
/// 从 Content 目录收集重要的 .uasset/.umap 文件。
fn collect_candidate_assets(content_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();

    for content_dir in content_dirs {
        let walker = ignore::WalkBuilder::new(content_dir)
            .hidden(false)
            .git_ignore(false)
            .follow_links(true)
            .filter_entry(|entry| !is_ignored_dir(entry.path()))
            .build();

        for entry in walker.filter_map(|entry| entry.ok()) {
            let path = entry.path();

            if !entry.file_type().map_or(false, |ty| ty.is_file()) {
                continue;
            }

            if !is_unreal_asset_file(path) {
                continue;
            }

            if !is_important_asset(path) {
                continue;
            }

            let normalized = normalize_path(path);
            if seen.insert(normalized) {
                files.push(path.to_path_buf());
            }
        }
    }

    files
}

/// Parse one asset file into an AssetRecord.
/// 把单个资产文件解析成 AssetRecord。
fn parse_asset_record(path: &Path) -> Result<AssetRecord> {
    let path = path.to_path_buf();

    let parse_result = std::panic::catch_unwind(move || {
        let mut parser = UAssetParser::new();
        parser
            .parse(&path)
            .map(|_| AssetRecord {
                asset_path: to_asset_path(&path),
                parent_class: parser.parent_class,
                imports: parser.imports,
                functions: parser.functions,
            })
    });

    match parse_result {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("panic while parsing asset")),
    }
}

// -----------------------------------------------------------------------------
// Graph building
// -----------------------------------------------------------------------------

/// Build a complete AssetGraph from parsed records.
/// 根据解析结果构建完整 AssetGraph。
fn build_asset_graph(records: Vec<AssetRecord>) -> AssetGraph {
    let mut graph = AssetGraph::default();

    for record in records {
        insert_asset_record(&mut graph, record);
    }

    graph
}

/// Insert one parsed asset record into the graph.
/// 把单个资产记录写入资产图。
fn insert_asset_record(graph: &mut AssetGraph, record: AssetRecord) {
    let asset_key: Arc<str> = record.asset_path.to_ascii_lowercase().into();

    if let Some(parent) = record.parent_class {
        graph
            .derived
            .entry(parent.to_ascii_lowercase().into())
            .or_default()
            .insert(asset_key.clone());
    }

    for import in record.imports {
        graph
            .references
            .entry(import.to_ascii_lowercase().into())
            .or_default()
            .insert(asset_key.clone());
    }

    for function in record.functions {
        graph
            .functions
            .entry(function.to_ascii_lowercase().into())
            .or_default()
            .insert(asset_key.clone());
    }
}

/// Remove an asset from all graph indexes before incremental reinsert.
/// 增量更新前，先从所有索引里移除旧的资产记录。
fn remove_asset_from_graph(graph: &mut AssetGraph, asset_path: &str) {
    let asset_key = asset_path.to_ascii_lowercase();

    for assets in graph.derived.values_mut() {
        assets.retain(|item| item.as_ref() != asset_key);
    }

    for assets in graph.references.values_mut() {
        assets.retain(|item| item.as_ref() != asset_key);
    }

    for assets in graph.functions.values_mut() {
        assets.retain(|item| item.as_ref() != asset_key);
    }
}

// -----------------------------------------------------------------------------
// Filters and path helpers
// -----------------------------------------------------------------------------

/// Return true if a directory should be ignored during asset scan.
/// 判断扫描资产时是否应该跳过某个目录。
fn is_ignored_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    matches!(
        name,
        "Intermediate" | "Binaries" | "Build" | "Saved" | ".git" | ".vs" | "DerivedDataCache"
    )
}

/// Return true for .uasset and .umap files.
/// 判断是否是 Unreal 资产文件。
fn is_unreal_asset_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase()),
        Some(ext) if ext == "uasset" || ext == "umap"
    )
}

/// Return true for assets that are worth parsing for navigation/search.
/// 判断资产是否值得解析，用于导航和搜索。
fn is_important_asset(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

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

/// Convert a filesystem path to Unreal asset path.
/// 把文件系统路径转换成 Unreal 资产路径。
pub fn to_asset_path(path: &Path) -> String {
    let normalized = normalize_path(path);

    if let Some(index) = normalized.find("/Content/") {
        let sub_path = &normalized[index + "/Content/".len()..];
        let without_ext = sub_path
            .rsplit_once('.')
            .map(|(base, _)| base)
            .unwrap_or(sub_path);

        return format!("/Game/{}", without_ext);
    }

    normalized
}

/// Normalize a path to slash-separated string.
/// 把路径统一成斜杠分隔字符串。
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").replace("//", "/")
}
