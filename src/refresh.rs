use anyhow::{anyhow, Result};
use ignore::{WalkBuilder, WalkState};
use rayon::prelude::*;
use regex::Regex;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tree_sitter::Query;

use crate::db;
use crate::db::project_path::get_or_create_directory;
use crate::types::{
    ComponentDef, InputFile, ModuleDef, ParseResult, PhaseInfo, ProgressReporter, RefreshRequest,
};
use crate::scanner;

const SOURCE_EXTENSIONS: &[&str] = &["h", "hh", "hpp", "cpp", "cc", "c", "cxx", "inl"];
const BUILD_CS_SUFFIX: &str = ".build.cs";

/// Run a full project refresh.
/// 执行一次完整工程刷新。
pub fn run_refresh(req: RefreshRequest, reporter: Arc<dyn ProgressReporter>) -> Result<()> {
    let ctx = RefreshContext::new(req)?;

    if !ctx.project_root.exists() {
        return Err(anyhow!("Project root does not exist: {}", ctx.project_root.display()));
    }

    report_plan(reporter.as_ref());
    reporter.report("discovery", 0, 100, &format!("Scanning: {}", ctx.project_root.display()));

    let ue_version = ctx.engine_root.as_deref().and_then(read_ue_version);
    let discovery = discover_project(&ctx, reporter.clone())?;

    reporter.report("discovery", 70, 100, "Resolving module dependencies...");
    let resolved_modules = resolve_modules(discovery.modules);

    reporter.report("db_sync", 0, 100, "Preparing database...");
    let mut conn = open_refresh_db(&ctx.db_path_native)?;
    write_engine_version(&conn, ue_version)?;

    let existing_files = load_existing_files(&conn)?;
    let module_map = write_components_and_modules(
        &mut conn,
        &ctx.project_root,
        &discovery.components,
        &resolved_modules,
    )?;

    let plan = build_file_plan(
        discovery.files,
        existing_files,
        module_map,
        ctx.project_root.clone(),
    );

    remove_deleted_files(&mut conn, &plan.deleted)?;

    parse_changed_sources(&mut conn, plan.sources_to_parse, reporter.clone())?;
    upsert_non_source_files(&mut conn, plan.other_files)?;

    reporter.report("complete", 100, 100, "Refresh complete.");
    Ok(())
}

// -----------------------------------------------------------------------------
// Context and discovery
// -----------------------------------------------------------------------------

/// Refresh configuration normalized for internal use.
/// refresh 内部使用的规范化配置。
struct RefreshContext {
    project_root: PathBuf,
    engine_root: Option<PathBuf>,
    db_path_native: String,
    scope: String,
    excludes: HashSet<String>,
    include_extensions: HashSet<String>,
}

/// Discovered project data before DB sync.
/// DB 同步前发现到的工程数据。
struct DiscoveryResult {
    components: Vec<ComponentDef>,
    modules: Vec<ModuleDef>,
    files: Vec<DiscoveredFile>,
}

/// One discovered file.
/// 扫描发现的单个文件。
struct DiscoveredFile {
    path: String,
    extension: String,
}

/// Refresh file plan after comparing disk and DB state.
/// 对比磁盘和 DB 后得到的文件处理计划。
struct RefreshFilePlan {
    sources_to_parse: Vec<InputFile>,
    other_files: Vec<FileUpsert>,
    deleted: Vec<String>,
}

/// Non-source file upsert data.
/// 非源码文件写入 files 表所需数据。
struct FileUpsert {
    path: String,
    extension: String,
    mtime: i64,
    module_id: i64,
}

impl RefreshContext {
    /// Normalize request into refresh context.
    /// 把请求规范化成 refresh 上下文。
    fn new(req: RefreshRequest) -> Result<Self> {
        let db_path = req
            .db_path
            .as_ref()
            .ok_or_else(|| anyhow!("DB path required for refresh"))?;

        let project_root = PathBuf::from(to_native_path(&req.project_root));
        let engine_root = req.engine_root.as_ref().map(|path| PathBuf::from(to_native_path(path)));

        Ok(Self {
            project_root,
            engine_root,
            db_path_native: to_native_path(db_path),
            scope: req.scope.unwrap_or_else(|| "Full".to_string()),
            excludes: req
                .config
                .excludes_directory
                .into_iter()
                .map(|item| item.to_ascii_lowercase())
                .collect(),
            include_extensions: req
                .config
                .include_extensions
                .into_iter()
                .map(|item| item.trim_start_matches('.').to_ascii_lowercase())
                .collect(),
        })
    }

    /// Return roots that should be scanned.
    /// 返回本次需要扫描的根目录。
    fn search_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.project_root.clone()];

        if matches!(self.scope.as_str(), "Full" | "Engine") {
            if let Some(engine_root) = &self.engine_root {
                roots.push(engine_root.clone());
            }
        }

        roots
    }
}

/// Report refresh phase plan.
/// 上报 refresh 阶段计划。
fn report_plan(reporter: &dyn ProgressReporter) {
    reporter.report_plan(&[
        PhaseInfo {
            name: "discovery".to_string(),
            label: "Discovery".to_string(),
            weight: 0.05,
        },
        PhaseInfo {
            name: "db_sync".to_string(),
            label: "DB Sync".to_string(),
            weight: 0.15,
        },
        PhaseInfo {
            name: "analysis".to_string(),
            label: "Analysis".to_string(),
            weight: 0.65,
        },
        PhaseInfo {
            name: "finalizing".to_string(),
            label: "Finalizing".to_string(),
            weight: 0.15,
        },
    ]);
}

/// Discover components, modules, and files.
/// 扫描工程，发现 component、module 和文件。
fn discover_project(ctx: &RefreshContext, reporter: Arc<dyn ProgressReporter>) -> Result<DiscoveryResult> {
    let project_name = root_name(&ctx.project_root);
    let engine_name = ctx.engine_root.as_deref().map(root_name);

    let mut components = base_components(ctx, &project_name, engine_name.as_deref());
    let mut modules = virtual_modules(ctx, &project_name, engine_name.as_deref());

    let discovered_files = Arc::new(parking_lot::Mutex::new(Vec::<DiscoveredFile>::new()));
    let build_files = Arc::new(parking_lot::Mutex::new(Vec::<(PathBuf, String)>::new()));
    let plugin_components = Arc::new(parking_lot::Mutex::new(Vec::<ComponentDef>::new()));
    let seen_count = Arc::new(AtomicUsize::new(0));

    let mut builder = WalkBuilder::new(ctx.search_roots().first().unwrap());
    for root in ctx.search_roots().iter().skip(1) {
        builder.add(root);
    }

    builder.hidden(false).git_ignore(false);

    let excludes = Arc::new(ctx.excludes.clone());
    let include_extensions = Arc::new(ctx.include_extensions.clone());
    let project_root = Arc::new(ctx.project_root.clone());
    let engine_root = Arc::new(ctx.engine_root.clone());
    let project_name = Arc::new(project_name);
    let engine_name = Arc::new(engine_name);

    builder.filter_entry({
        let excludes = excludes.clone();

        move |entry| {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            !excludes.contains(&name)
        }
    });

    builder.build_parallel().run(|| {
        let discovered_files = discovered_files.clone();
        let build_files = build_files.clone();
        let plugin_components = plugin_components.clone();
        let include_extensions = include_extensions.clone();
        let project_root = project_root.clone();
        let engine_root = engine_root.clone();
        let project_name = project_name.clone();
        let engine_name = engine_name.clone();
        let seen_count = seen_count.clone();
        let reporter = reporter.clone();

        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => return WalkState::Continue,
            };

            let count = seen_count.fetch_add(1, Ordering::Relaxed) + 1;
            if count % 1000 == 0 {
                let current = (count / 1000).clamp(1, 69);
                reporter.report(
                    "discovery",
                    current,
                    100,
                    &format!("Discovery: {} files seen", count),
                );
            }

            let path = entry.path();
            let extension = file_extension(path);

            if extension == "uplugin" {
                if let Some(component) = plugin_component(path, &project_root, engine_root.as_deref(), &project_name, engine_name.as_deref()) {
                    plugin_components.lock().push(component);
                }
            }

            if is_build_cs(path) {
                let owner = owner_name_for_path(path, &project_root, engine_root.as_deref(), &project_name, engine_name.as_deref());
                build_files.lock().push((path.to_path_buf(), owner));
            }

            if entry.file_type().map_or(false, |ty| ty.is_file()) && include_extensions.contains(&extension) {
                discovered_files.lock().push(DiscoveredFile {
                    path: normalize_path(path),
                    extension,
                });
            }

            WalkState::Continue
        })
    });

    components.extend(plugin_components.lock().drain(..));
    components = dedupe_components(components);

    let sorted_components = components_sorted_by_depth(&components);

    for (build_path, owner) in build_files.lock().drain(..) {
        if let Some(module) = build_file_to_module(&build_path, &owner, &sorted_components) {
            modules.push(module);
        }
    }

    Ok(DiscoveryResult {
        components,
        modules,
        files: discovered_files.lock().drain(..).collect(),
    })
}

/// Create base game/engine components.
/// 创建基础 Game/Engine component。
fn base_components(
    ctx: &RefreshContext,
    project_name: &str,
    engine_name: Option<&str>,
) -> Vec<ComponentDef> {
    let mut components = vec![ComponentDef {
        name: project_name.to_string(),
        display_name: ctx
            .project_root
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| project_name.to_string()),
        comp_type: "Game".to_string(),
        root_path: ctx.project_root.clone(),
        uproject_path: find_uproject(&ctx.project_root),
        uplugin_path: None,
        owner_name: project_name.to_string(),
    }];

    if let (Some(engine_root), Some(engine_name)) = (&ctx.engine_root, engine_name) {
        components.push(ComponentDef {
            name: engine_name.to_string(),
            display_name: "Engine".to_string(),
            comp_type: "Engine".to_string(),
            root_path: engine_root.clone(),
            uproject_path: None,
            uplugin_path: None,
            owner_name: engine_name.to_string(),
        });
    }

    components
}

/// Create virtual config/shader modules.
/// 创建虚拟 Config/Shader 模块。
fn virtual_modules(
    ctx: &RefreshContext,
    project_name: &str,
    engine_name: Option<&str>,
) -> Vec<ModuleDef> {
    let mut modules = vec![ModuleDef {
        name: "_GameConfig".to_string(),
        path: ctx.project_root.join("Config"),
        root: ctx.project_root.join("Config"),
        public_deps: vec![],
        private_deps: vec![],
        mod_type: "Config".to_string(),
        owner_name: project_name.to_string(),
        component_name: Some(project_name.to_string()),
    }];

    if let (Some(engine_root), Some(engine_name)) = (&ctx.engine_root, engine_name) {
        modules.push(ModuleDef {
            name: "_EngineConfig".to_string(),
            path: engine_root.join("Engine/Config"),
            root: engine_root.join("Engine/Config"),
            public_deps: vec![],
            private_deps: vec![],
            mod_type: "Config".to_string(),
            owner_name: engine_name.to_string(),
            component_name: Some(engine_name.to_string()),
        });

        modules.push(ModuleDef {
            name: "_EngineShaders".to_string(),
            path: engine_root.join("Engine/Shaders"),
            root: engine_root.join("Engine/Shaders"),
            public_deps: vec![],
            private_deps: vec![],
            mod_type: "Shader".to_string(),
            owner_name: engine_name.to_string(),
            component_name: Some(engine_name.to_string()),
        });
    }

    modules
}

// -----------------------------------------------------------------------------
// DB sync
// -----------------------------------------------------------------------------

/// Open and initialize refresh DB.
/// 打开并初始化 refresh 数据库。
fn open_refresh_db(db_path: &str) -> Result<Connection> {
    db::ensure_correct_version(db_path)?;

    let conn = Connection::open(db_path)?;
    conn.busy_timeout(std::time::Duration::from_millis(10_000))?;
    db::init_db(&conn)?;

    Ok(conn)
}

/// Write Unreal Engine version metadata.
/// 写入 Unreal Engine 版本元数据。
fn write_engine_version(conn: &Connection, version: Option<UeBuildVersion>) -> Result<()> {
    let Some(version) = version else {
        return Ok(());
    };

    for (key, value) in [
        ("ue_version_major", version.major.to_string()),
        ("ue_version_minor", version.minor.to_string()),
        ("ue_version_patch", version.patch.to_string()),
        ("ue_version_branch", version.branch),
    ] {
        conn.execute(
            "INSERT OR REPLACE INTO project_meta (key, value) VALUES (?, ?)",
            params![key, value],
        )?;
    }

    Ok(())
}

/// Load known files and mtimes from DB.
/// 从 DB 读取已有文件路径和 mtime。
fn load_existing_files(conn: &Connection) -> Result<HashMap<String, i64>> {
    let mut dir_map = HashMap::new();

    {
        let mut stmt = conn.prepare(
            "SELECT d.id, d.parent_id, s.text FROM directories d JOIN strings s ON d.name_id = s.id",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        for row in rows {
            let (id, parent, name) = row?;
            dir_map.insert(id, (parent, name));
        }
    }

    let mut files = HashMap::new();

    let mut stmt = conn.prepare(
        "SELECT f.directory_id, s.text, f.mtime FROM files f JOIN strings s ON f.filename_id = s.id",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    for row in rows {
        let (dir_id, filename, mtime) = row?;
        files.insert(reconstruct_path(&dir_map, dir_id, &filename), mtime);
    }

    Ok(files)
}

/// Write components and modules, then return module root -> module id map.
/// 写入 components/modules，并返回 module root -> module id 映射。
fn write_components_and_modules(
    conn: &mut Connection,
    project_root: &Path,
    components: &[ComponentDef],
    modules: &[(ModuleDef, HashSet<String>)],
) -> Result<HashMap<String, i64>> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM components", [])?;

    let mut string_cache = HashMap::new();
    let mut dir_cache = HashMap::new();

    for component in components {
        tx.execute(
            r#"
            INSERT INTO components
                (name, display_name, type, owner_name, root_path, uplugin_path, uproject_path)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                component.name,
                component.display_name,
                component.comp_type,
                component.owner_name,
                normalize_path(&component.root_path),
                component.uplugin_path.as_ref().map(|path| normalize_path(path)),
                component.uproject_path.as_ref().map(|path| normalize_path(path)),
            ],
        )?;
    }

    let mut module_map = HashMap::new();

    for (module, deep_deps) in modules {
        let name_id = db::get_or_create_string(&tx, &mut string_cache, &module.name)?;
        let root_dir_id =
            get_or_create_directory(&tx, &mut string_cache, &mut dir_cache, &module.root)?;

        tx.execute(
            r#"
            INSERT INTO modules
                (name_id, type, scope, root_directory_id, build_cs_path, owner_name, component_name, deep_dependencies)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(name_id, root_directory_id) DO UPDATE SET
                type = excluded.type,
                scope = excluded.scope,
                build_cs_path = excluded.build_cs_path,
                owner_name = excluded.owner_name,
                component_name = excluded.component_name,
                deep_dependencies = excluded.deep_dependencies
            "#,
            params![
                name_id,
                module.mod_type,
                "Individual",
                root_dir_id,
                normalize_path(&module.path),
                module.owner_name,
                module.component_name,
                serde_json::to_string(&sorted_set(deep_deps))?,
            ],
        )?;

        let module_id = tx.query_row(
            "SELECT id FROM modules WHERE name_id = ? AND root_directory_id = ?",
            params![name_id, root_dir_id],
            |row| row.get::<_, i64>(0),
        )?;

        module_map.insert(normalize_path(&module.root), module_id);
    }

    let global_id = insert_global_module(&tx, project_root, &mut string_cache, &mut dir_cache)?;
    module_map.insert(normalize_path(project_root), global_id);

    tx.commit()?;
    Ok(module_map)
}

/// Insert fallback _Global module.
/// 写入兜底 _Global 模块。
fn insert_global_module(
    tx: &rusqlite::Transaction,
    project_root: &Path,
    string_cache: &mut HashMap<String, i64>,
    dir_cache: &mut HashMap<(Option<i64>, i64), i64>,
) -> Result<i64> {
    let name_id = db::get_or_create_string(tx, string_cache, "_Global")?;
    let root_dir_id = get_or_create_directory(tx, string_cache, dir_cache, project_root)?;

    tx.execute(
        r#"
        INSERT INTO modules (name_id, type, scope, root_directory_id)
        VALUES (?, ?, ?, ?)
        ON CONFLICT(name_id, root_directory_id) DO UPDATE SET
            type = excluded.type,
            scope = excluded.scope
        "#,
        params![name_id, "Global", "Game", root_dir_id],
    )?;

    Ok(tx.query_row(
        "SELECT id FROM modules WHERE name_id = ? AND root_directory_id = ?",
        params![name_id, root_dir_id],
        |row| row.get::<_, i64>(0),
    )?)
}

// -----------------------------------------------------------------------------
// File planning and parsing
// -----------------------------------------------------------------------------

/// Build parsing/upsert/deletion plan.
/// 构造解析、写入和删除计划。
fn build_file_plan(
    files: Vec<DiscoveredFile>,
    existing: HashMap<String, i64>,
    module_map: HashMap<String, i64>,
    project_root: PathBuf,
) -> RefreshFilePlan {
    let sorted_modules = sorted_module_roots(module_map);
    let global_module_id = sorted_modules
        .iter()
        .find(|(root, _)| root == &normalize_path(&project_root))
        .map(|(_, id)| *id)
        .unwrap_or(0);

    let mut on_disk = HashSet::new();
    let mut sources_to_parse = Vec::new();
    let mut other_files = Vec::new();

    for file in files {
        on_disk.insert(file.path.clone());

        let mtime = file_mtime(&file.path);
        let module_id = sorted_modules
            .iter()
            .find(|(root, _)| file.path.starts_with(root))
            .map(|(_, id)| *id)
            .unwrap_or(global_module_id);

        let changed = existing.get(&file.path).copied() != Some(mtime);

        if changed && is_source_extension(&file.extension) {
            sources_to_parse.push(InputFile {
                path: file.path,
                mtime: mtime as u64,
                old_hash: None,
                module_id: Some(module_id),
                db_path: None,
            });
        } else {
            other_files.push(FileUpsert {
                path: file.path,
                extension: file.extension,
                mtime,
                module_id,
            });
        }
    }

    let deleted = existing
        .keys()
        .filter(|path| !on_disk.contains(*path))
        .cloned()
        .collect();

    RefreshFilePlan {
        sources_to_parse,
        other_files,
        deleted,
    }
}

/// Remove files missing from disk.
/// 删除磁盘上已经不存在的文件。
fn remove_deleted_files(conn: &mut Connection, deleted: &[String]) -> Result<()> {
    if deleted.is_empty() {
        return Ok(());
    }

    let tx = conn.transaction()?;
    let mut string_cache = HashMap::new();
    let mut dir_cache = HashMap::new();

    for path in deleted {
        let p = Path::new(path);
        let parent = p.parent().unwrap_or_else(|| Path::new(""));
        let filename = p.file_name().and_then(|name| name.to_str()).unwrap_or("");

        if filename.is_empty() {
            continue;
        }

        let dir_id = get_or_create_directory(&tx, &mut string_cache, &mut dir_cache, parent)?;
        let filename_id = db::get_or_create_string(&tx, &mut string_cache, filename)?;

        tx.execute(
            "DELETE FROM files WHERE directory_id = ? AND filename_id = ?",
            params![dir_id, filename_id],
        )?;
    }

    tx.commit()?;
    Ok(())
}

/// Parse changed source files in parallel.
/// 并行解析变更过的源码文件。
fn parse_changed_sources(
    conn: &mut Connection,
    files: Vec<InputFile>,
    reporter: Arc<dyn ProgressReporter>,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    reporter.report(
        "analysis",
        0,
        files.len(),
        &format!("Analyzing {} files...", files.len()),
    );

    let language = tree_sitter_unreal_cpp::LANGUAGE.into();
    let query = Arc::new(Query::new(&language, scanner::QUERY_STR)?);
    let include_query = Arc::new(Query::new(&language, scanner::INCLUDE_QUERY_STR)?);

    let total = files.len();
    let processed = AtomicUsize::new(0);
    let reported_percent = AtomicUsize::new(0);

    let results = files
        .into_par_iter()
        .map(|input| {
            let result = scanner::process_file(&input, &language, &query, &include_query)
                .unwrap_or_else(|_| ParseResult {
                    path: input.path,
                    status: "error".to_string(),
                    mtime: input.mtime,
                    data: None,
                    module_id: input.module_id,
                });

            let current = processed.fetch_add(1, Ordering::Relaxed) + 1;
            let percent = (current * 100 / total).min(100);
            let previous = reported_percent.load(Ordering::Relaxed);

            if current == total
                || (percent > previous
                    && reported_percent
                        .compare_exchange(previous, percent, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok())
            {
                reporter.report(
                    "analysis",
                    current,
                    total,
                    &format!("Analyzing: {}/{}", current, total),
                );
            }

            result
        })
        .collect::<Vec<_>>();

    db::save_to_db(conn, &results, reporter)?;
    Ok(())
}

/// Upsert non-source or unchanged files.
/// 写入非源码文件或未变更文件。
fn upsert_non_source_files(conn: &mut Connection, files: Vec<FileUpsert>) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let tx = conn.transaction()?;
    let mut string_cache = HashMap::new();
    let mut dir_cache = HashMap::new();

    for file in files {
        let path = Path::new(&file.path);
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        let filename = path.file_name().and_then(|name| name.to_str()).unwrap_or("");

        if filename.is_empty() {
            continue;
        }

        let dir_id = get_or_create_directory(&tx, &mut string_cache, &mut dir_cache, parent)?;
        let filename_id = db::get_or_create_string(&tx, &mut string_cache, filename)?;

        tx.execute(
            r#"
            INSERT OR REPLACE INTO files
                (directory_id, filename_id, extension, mtime, module_id, is_header)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
            params![
                dir_id,
                filename_id,
                file.extension,
                file.mtime,
                file.module_id,
                is_header_extension(&file.extension) as i64,
            ],
        )?;
    }

    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// Module dependency resolution
// -----------------------------------------------------------------------------

/// Resolve transitive module dependencies.
/// 解析模块的传递依赖。
fn resolve_modules(modules: Vec<ModuleDef>) -> Vec<(ModuleDef, HashSet<String>)> {
    let lookup = modules
        .iter()
        .map(|module| (module.name.clone(), module.clone()))
        .collect::<HashMap<_, _>>();

    let mut memo = HashMap::new();

    modules
        .into_iter()
        .map(|module| {
            let mut stack = Vec::new();
            let deep = resolve_deep(&module.name, &lookup, &mut memo, &mut stack);
            (module, deep)
        })
        .collect()
}

/// Recursive dependency resolver with cycle guard.
/// 带循环保护的递归依赖解析。
fn resolve_deep(
    name: &str,
    lookup: &HashMap<String, ModuleDef>,
    memo: &mut HashMap<String, HashSet<String>>,
    stack: &mut Vec<String>,
) -> HashSet<String> {
    if let Some(cached) = memo.get(name) {
        return cached.clone();
    }

    if stack.iter().any(|item| item == name) {
        return HashSet::new();
    }

    stack.push(name.to_string());

    let mut result = HashSet::new();

    if let Some(module) = lookup.get(name) {
        for dep in module.public_deps.iter().chain(module.private_deps.iter()) {
            result.insert(dep.clone());

            for nested in resolve_deep(dep, lookup, memo, stack) {
                result.insert(nested);
            }
        }
    }

    stack.pop();
    memo.insert(name.to_string(), result.clone());

    result
}

// -----------------------------------------------------------------------------
// Build.cs parsing
// -----------------------------------------------------------------------------

/// Parse one .Build.cs file for Public/PrivateDependencyModuleNames.
/// 解析一个 .Build.cs 里的 Public/PrivateDependencyModuleNames。
fn parse_build_cs(path: &Path) -> (Vec<String>, Vec<String>) {
    let content = fs::read_to_string(path).unwrap_or_default();

    let add_range = Regex::new(
        r#"(Public|Private)DependencyModuleNames\s*\.\s*AddRange\s*\(\s*new\s+string\s*\[\]\s*\{(?s:(.*?))\}\s*\)"#,
    )
    .unwrap();

    let add_single = Regex::new(
        r#"(Public|Private)DependencyModuleNames\s*\.\s*Add\s*\(\s*"([^"]+)"\s*\)"#,
    )
    .unwrap();

    let quoted = Regex::new(r#""([^"]+)""#).unwrap();

    let mut public_deps = Vec::new();
    let mut private_deps = Vec::new();

    for cap in add_range.captures_iter(&content) {
        let target = if &cap[1] == "Public" {
            &mut public_deps
        } else {
            &mut private_deps
        };

        for module in quoted.captures_iter(&cap[2]) {
            target.push(module[1].to_string());
        }
    }

    for cap in add_single.captures_iter(&content) {
        if &cap[1] == "Public" {
            public_deps.push(cap[2].to_string());
        } else {
            private_deps.push(cap[2].to_string());
        }
    }

    public_deps.sort();
    public_deps.dedup();

    private_deps.sort();
    private_deps.dedup();

    (public_deps, private_deps)
}

/// Convert a .Build.cs path into a module definition.
/// 把 .Build.cs 路径转换成 ModuleDef。
fn build_file_to_module(
    build_path: &Path,
    owner: &str,
    components: &[ComponentDef],
) -> Option<ModuleDef> {
    let root = build_path.parent()?.to_path_buf();
    let name = build_path.file_name()?.to_string_lossy().split('.').next()?.to_string();
    let (public_deps, private_deps) = parse_build_cs(build_path);

    let component_name = components
        .iter()
        .find(|component| root.starts_with(&component.root_path))
        .map(|component| component.name.clone());

    Some(ModuleDef {
        name,
        path: build_path.to_path_buf(),
        root,
        public_deps,
        private_deps,
        mod_type: "Runtime".to_string(),
        owner_name: owner.to_string(),
        component_name,
    })
}

// -----------------------------------------------------------------------------
// UE version
// -----------------------------------------------------------------------------

#[derive(serde::Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct UeBuildVersion {
    major: i32,
    minor: i32,
    patch: i32,
    branch: String,
}

/// Read Engine/Build/Build.version.
/// 读取 Engine/Build/Build.version。
fn read_ue_version(engine_root: &Path) -> Option<UeBuildVersion> {
    let path = engine_root.join("Engine/Build/Build.version");
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

// -----------------------------------------------------------------------------
// Path and file helpers
// -----------------------------------------------------------------------------

fn find_uproject(project_root: &Path) -> Option<PathBuf> {
    fs::read_dir(project_root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| file_extension(path) == "uproject")
}

fn plugin_component(
    uplugin_path: &Path,
    project_root: &Path,
    _engine_root: Option<&Path>,
    project_name: &str,
    engine_name: Option<&str>,
) -> Option<ComponentDef> {
    let root = uplugin_path.parent()?.to_path_buf();

    let owner = if root.starts_with(project_root) {
        project_name.to_string()
    } else {
        engine_name.unwrap_or("Engine").to_string()
    };

    Some(ComponentDef {
        name: root_name(&root),
        display_name: uplugin_path.file_stem()?.to_string_lossy().to_string(),
        comp_type: "Plugin".to_string(),
        root_path: root,
        uproject_path: None,
        uplugin_path: Some(uplugin_path.to_path_buf()),
        owner_name: owner,
    })
}

fn owner_name_for_path(
    path: &Path,
    project_root: &Path,
    engine_root: Option<&Path>,
    project_name: &str,
    engine_name: Option<&str>,
) -> String {
    if engine_root.map(|root| path.starts_with(root)).unwrap_or(false) {
        engine_name.unwrap_or("Engine").to_string()
    } else if path.starts_with(project_root) {
        project_name.to_string()
    } else {
        "Unknown".to_string()
    }
}

fn components_sorted_by_depth(components: &[ComponentDef]) -> Vec<ComponentDef> {
    let mut result = components.to_vec();
    result.sort_by(|a, b| b.root_path.as_os_str().len().cmp(&a.root_path.as_os_str().len()));
    result
}

fn dedupe_components(components: Vec<ComponentDef>) -> Vec<ComponentDef> {
    let mut seen = HashSet::new();
    components
        .into_iter()
        .filter(|component| seen.insert(component.name.clone()))
        .collect()
}

fn sorted_module_roots(module_map: HashMap<String, i64>) -> Vec<(String, i64)> {
    let mut roots = module_map.into_iter().collect::<Vec<_>>();
    roots.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    roots
}

fn is_build_cs(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase().ends_with(BUILD_CS_SUFFIX))
        .unwrap_or(false)
}

fn is_source_extension(ext: &str) -> bool {
    SOURCE_EXTENSIONS.contains(&ext)
}

fn is_header_extension(ext: &str) -> bool {
    matches!(ext, "h" | "hh" | "hpp" | "inl")
}

fn file_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn file_mtime(path: &str) -> i64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn root_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Unknown")
        .to_string()
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn to_native_path(path: &str) -> String {
    if cfg!(target_os = "windows") {
        path.replace('/', "\\")
    } else {
        path.replace('\\', "/")
    }
}

fn sorted_set(set: &HashSet<String>) -> Vec<String> {
    let mut items = set.iter().cloned().collect::<Vec<_>>();
    items.sort();
    items
}

/// Reconstruct a full path from directory map and filename.
/// 从目录 map 和文件名重建完整路径。
fn reconstruct_path(
    dir_map: &HashMap<i64, (Option<i64>, String)>,
    mut dir_id: i64,
    filename: &str,
) -> String {
    let mut segments = vec![filename.to_string()];

    while let Some((parent, name)) = dir_map.get(&dir_id) {
        segments.push(name.clone());

        let Some(parent_id) = parent else {
            break;
        };

        dir_id = *parent_id;
    }

    segments.reverse();
    segments.join("/")
}
