use anyhow::{anyhow, Result};
use notify::Watcher;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::server::asset::handle_asset_scan;
use crate::server::state::{AppState, ProjectContext, RpcProgressReporter};
use crate::server::utils::{
    convert_params, normalize_path_key, normalize_to_native, normalize_to_unix,
};
use crate::types::{
    ModifyResult, ModifyTargetAddModuleRequest, ModifyUprojectAddModuleRequest, QueryRequest,
    RefreshRequest, ScanRequest, SetupRequest,
};
use crate::{db, query, refresh, scanner};

const SERVER_PROTOCOL_VERSION: u32 = 2;

// -----------------------------------------------------------------------------
// Request types
// -----------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct DeleteProjectRequest {
    pub project_root: String,
}

#[derive(Deserialize)]
pub struct PingRequest {
    pub pid: u32,
}

#[derive(Deserialize)]
pub struct ServerQueryRequest {
    pub project_root: String,

    #[serde(default)]
    pub engine_db_path: Option<String>,

    #[serde(flatten)]
    pub query: QueryRequest,
}

// -----------------------------------------------------------------------------
// Project lifecycle handlers
// -----------------------------------------------------------------------------

/// Delete a registered project from server state.
/// 从 server 状态里删除一个已注册工程。
pub async fn handle_delete_project(state: &AppState, params: &Value) -> Result<Value> {
    let req: DeleteProjectRequest = convert_params(params)?;
    let root_key = normalize_path_key(&req.project_root);

    let removed = {
        let mut projects = state.projects.lock();
        projects.remove(&root_key).is_some()
    };

    if !removed {
        return Err(anyhow!("Project not found: {}", root_key));
    }

    let _ = state.save_registry();
    info!("Deleted project: {}", root_key);

    Ok(Value::String("Deleted".to_string()))
}

/// Register a Neovim client heartbeat.
/// 注册 Neovim 客户端心跳。
pub async fn handle_ping(state: &AppState, params: &Value) -> Result<Value> {
    let req: PingRequest = convert_params(params)?;
    state.register_client(req.pid);
    Ok(Value::String("pong".to_string()))
}

/// Setup one project and open/create its database.
/// 初始化一个工程，并打开或创建对应数据库。
pub async fn handle_setup(state: Arc<AppState>, params: &Value) -> Result<Value> {
    let req: SetupRequest = convert_params(params)?;

    let root_key = normalize_path_key(&req.project_root);
    let db_path_unix = normalize_to_unix(&req.db_path);
    let db_path_native = normalize_to_native(&req.db_path);
    let cache_db_path_unix = req.cache_db_path.as_ref().map(|p| normalize_to_unix(p));

    drop_db_connections(&state, &db_path_native, cache_db_path_unix.as_deref());

    let needs_full_refresh = ensure_database_ready(db_path_native.clone()).await?;

    {
        let mut projects = state.projects.lock();
        projects.insert(
            root_key.clone(),
            ProjectContext {
                db_path: db_path_unix,
                cache_db_path: cache_db_path_unix.clone(),
                vcs_hash: req.vcs_hash.clone(),
                last_refresh_at: Instant::now(),
            },
        );
    }

    let _ = state.get_connection(&db_path_native);

    if let Some(cache_path) = req.cache_db_path.as_ref() {
        let _ = state.get_persistent_cache_connection(&normalize_to_native(cache_path));
    }

    let _ = state.save_registry();

    spawn_asset_scan_if_needed(state.clone(), root_key.clone(), req.project_root.clone());

    Ok(json!({
        "status": "ok",
        "needs_full_refresh": needs_full_refresh,
    }))
}

/// Run a full project refresh.
/// 执行一次完整工程刷新。
pub async fn handle_refresh(
    state: &AppState,
    params: &Value,
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<Value> {
    let mut req: RefreshRequest = convert_params(params)?;
    let root_key = normalize_path_key(&req.project_root);

    let _guard = RefreshGuard::try_new(state, root_key.clone())?;

    let db_path_unix = upsert_refresh_project_context(state, &mut req, &root_key)?;
    let db_path_native = normalize_to_native(&db_path_unix);

    let cache_path = {
        let projects = state.projects.lock();
        projects
            .get(&root_key)
            .and_then(|ctx| ctx.cache_db_path.clone())
    };

    drop_db_connections(state, &db_path_native, cache_path.as_deref());

    req.db_path = Some(db_path_unix.clone());
    let _ = state.save_registry();

    let reporter = Arc::new(RpcProgressReporter { tx });

    tokio::task::spawn_blocking(move || refresh::run_refresh(req, reporter)).await??;

    clear_completion_cache(state, &root_key);

    let _ = state.get_connection(&db_path_native);

    Ok(Value::String("Refresh success".to_string()))
}

/// Start filesystem watcher for a project root.
/// 启动工程目录文件监听。
pub async fn handle_watch(state: &AppState, params: &Value) -> Result<Value> {
    let req: crate::types::WatchRequest = convert_params(params)?;
    let root_native = normalize_to_native(&req.project_root);
    let root_path = PathBuf::from(&root_native);

    if !root_path.exists() {
        return Err(anyhow!("Path does not exist: {}", root_native));
    }

    let mut watcher = state.watcher.lock();

    watcher
        .watch(&root_path, notify::RecursiveMode::Recursive)
        .map_err(|err| {
            error!("Watcher failed for {}: {}", root_native, err);
            err
        })?;

    info!("Watcher started: {}", root_native);
    Ok(Value::String("Watch started".to_string()))
}

// -----------------------------------------------------------------------------
// Query handlers
// -----------------------------------------------------------------------------

/// Handle one query request from Neovim.
/// 处理来自 Neovim 的一次 query 请求。
pub async fn handle_query(
    state: Arc<AppState>,
    params: &Value,
    tx: mpsc::Sender<Vec<u8>>,
    msgid: u64,
) -> Result<Value> {
    let req: ServerQueryRequest = convert_params(params)?;
    let root_key = normalize_path_key(&req.project_root);

    if is_refreshing(&state, &root_key) {
        return Ok(json!([]));
    }

    ensure_asset_scan_started(&state, &root_key, &req.project_root);

    let project = get_project_context(&state, &root_key)?;
    let db_path_native = normalize_to_native(&project.db_path);
    let cache_db_path_native = project.cache_db_path.as_ref().map(|p| normalize_to_native(p));

    let conn = state.get_read_only_connection(&db_path_native)?;
    let persistent_cache_conn = cache_db_path_native
        .as_deref()
        .and_then(|path| state.get_persistent_cache_connection(path).ok());

    tokio::task::spawn_blocking(move || {
        if let Some(value) = handle_state_query(
            state.clone(),
            &conn,
            &root_key,
            &req.project_root,
            req.engine_db_path.clone(),
            req.query.clone(),
            persistent_cache_conn,
        )? {
            return Ok(value);
        }

        if is_streaming_query(&req.query) {
            query::process_query_streaming(&conn, req.query, move |items| {
                send_query_partial(&tx, msgid, items)?;
                Ok(())
            })
        } else {
            query::process_query(&conn, req.query)
        }
    })
    .await?
}

/// Handle queries that need AppState instead of only SQLite.
/// 处理那些必须访问 AppState、不能只靠 SQLite 的 query。
fn handle_state_query(
    state: Arc<AppState>,
    conn: &rusqlite::Connection,
    root_key: &str,
    project_root: &str,
    engine_db_path: Option<String>,
    request: QueryRequest,
    persistent_cache_conn: Option<Arc<parking_lot::Mutex<rusqlite::Connection>>>,
) -> Result<Option<Value>> {
    match request {
        QueryRequest::SearchSymbols {
            pattern,
            limit,
            offset,
        } => {
            let value =
                search_symbols_with_engine(state, conn, engine_db_path, &pattern, limit, offset)?;
            Ok(Some(value))
        }
        QueryRequest::FastFind {
            pattern,
            limit,
            offset,
            scope,
        } => {
            let value = fast_find_with_scope(
                state,
                conn,
                engine_db_path,
                &pattern,
                limit,
                offset,
                scope.as_deref(),
            )?;
            Ok(Some(value))
        }
        QueryRequest::SearchCodeText {
            pattern,
            limit,
            offset,
            scope,
        } => {
            let value = search_code_text_with_scope(
                state,
                conn,
                engine_db_path,
                &pattern,
                limit,
                offset,
                scope.as_deref(),
            )?;
            Ok(Some(value))
        }
        QueryRequest::GlobalFind {
            pattern,
            limit,
            offset,
        } => {
            let value = global_find_with_engine(state, conn, engine_db_path, &pattern, limit, offset)?;
            Ok(Some(value))
        }

        QueryRequest::FindSymbolUsages {
            symbol_name,
            file_path,
            content,
            line,
            character,
        } => {
            let value = find_references_with_engine(
                state,
                conn,
                engine_db_path,
                &symbol_name,
                file_path.as_deref(),
                content.as_deref(),
                line,
                character,
            )?;
            Ok(Some(value))
        }

        QueryRequest::GotoDefinition {
            content,
            line,
            character,
            file_path,
        } => {
            let value = goto_definition_with_engine(
                state,
                conn,
                engine_db_path,
                content,
                line,
                character,
                file_path,
            )?;

            Ok(Some(value))
        }

        QueryRequest::GotoImplementation {
            content,
            line,
            character,
            file_path,
        } => {
            let value = goto_implementation_with_engine(
                state,
                conn,
                engine_db_path,
                content,
                line,
                character,
                file_path,
            )?;

            Ok(Some(value))
        }

        QueryRequest::GetHover {
            content,
            line,
            character,
            file_path,
        } => {
            let value = hover_with_engine(
                state,
                conn,
                engine_db_path,
                content,
                line,
                character,
                file_path,
            )?;

            Ok(Some(value))
        }

        QueryRequest::GetSignatureHelp {
            content,
            line,
            character,
            file_path,
        } => {
            let value = signature_help_with_engine(
                state,
                conn,
                engine_db_path,
                content,
                line,
                character,
                file_path,
            )?;

            Ok(Some(value))
        }

        QueryRequest::GetAssetUsages { asset_path } => {
            Ok(Some(get_asset_usages(&state, root_key, &asset_path)))
        }

        QueryRequest::GetAssetDependencies { asset_path } => {
            Ok(Some(get_asset_dependencies(project_root, &asset_path)?))
        }

        QueryRequest::FindDerivedClasses { base_class } => {
            let mut db_results = query::process_query(
                conn,
                QueryRequest::FindDerivedClasses {
                    base_class: base_class.clone(),
                },
            )?
            .as_array()
            .cloned()
            .unwrap_or_default();

            merge_asset_derived_classes(&state, root_key, &base_class, &mut db_results);

            Ok(Some(json!(db_results)))
        }

        QueryRequest::GetAssets => Ok(Some(get_assets_from_graph(&state, root_key))),

        QueryRequest::GetConfigData { engine_root } => {
            let data =
                query::config::get_config_data_with_cache(&state, project_root, engine_root.as_deref())?;
            Ok(Some(json!(data)))
        }

        QueryRequest::GetCompletions {
            content,
            line,
            character,
            file_path,
        } => {
            let file_path_display = file_path
                .as_deref()
                .unwrap_or("-")
                .to_string();
            let cache = state.get_completion_cache(root_key);
            let engine_conn = match engine_db_path
                .as_deref()
                .map(normalize_to_native)
                .filter(|path| Path::new(path).is_file())
            {
                Some(path) => match state.get_read_only_connection(&path) {
                    Ok(conn) => Some(conn),
                    Err(err) => {
                        warn!("Failed to open Engine DB for completions: {}", err);
                        None
                    }
                },
                None => None,
            };

            let value = crate::completion::process_completion_with_engine(
                conn,
                engine_conn.as_ref(),
                &content,
                line,
                character,
                file_path,
                Some(cache),
                persistent_cache_conn,
            )?;

            debug!(
                "completion query handled: root={} file={} line={} char={} engine_db={}",
                root_key,
                file_path_display,
                line,
                character,
                engine_conn.is_some(),
            );

            Ok(Some(value))
        }

        QueryRequest::GetDiagnostics {
            content,
            file_path,
            open_files,
        } => {
            let engine_conn = match engine_db_path
                .as_deref()
                .map(normalize_to_native)
                .filter(|path| Path::new(path).is_file())
            {
                Some(path) => match state.get_read_only_connection(&path) {
                    Ok(conn) => Some(conn),
                    Err(err) => {
                        warn!("Failed to open Engine DB for diagnostics: {}", err);
                        None
                    }
                },
                None => None,
            };

            let value = crate::diagnostics::process_diagnostics(
                conn,
                engine_conn.as_ref(),
                &content,
                file_path,
                &open_files,
            )?;
            Ok(Some(value))
        }

        QueryRequest::ParseBuildDiagnostics { output } => {
            Ok(Some(crate::diagnostics::parse_build_diagnostics(&output)))
        }

        _ => Ok(None),
    }
}

/// Return true for query variants that stream partial results.
/// 判断 query 是否需要流式分批返回。
fn is_streaming_query(request: &QueryRequest) -> bool {
    matches!(
        request,
        QueryRequest::GetFilesInModulesAsync { .. }
            | QueryRequest::SearchFilesInModulesAsync { .. }
            | QueryRequest::SearchFilesByPathPartAsync { .. }
            | QueryRequest::GetClassesInModulesAsync { .. }
            | QueryRequest::FindSymbolUsagesAsync { .. }
            | QueryRequest::GrepAssets { .. }
    )
}

/// Send one query partial notification through MessagePack RPC channel.
/// 通过 MessagePack RPC 通道发送一批 query partial 数据。
fn send_query_partial(tx: &mpsc::Sender<Vec<u8>>, msgid: u64, items: Vec<Value>) -> Result<()> {
    let notification = (2, "query/partial", json!({
        "msgid": msgid,
        "items": items,
    }));

    let payload = rmp_serde::to_vec(&notification)?;

    let mut framed = Vec::with_capacity(payload.len() + 4);
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(&payload);

    let _ = tx.blocking_send(framed);

    Ok(())
}

/// Go to definition in the project DB, then fall back to the shared Engine DB.
/// 先在项目 DB 里跳转定义，找不到时回退到共享 Engine DB。
fn goto_definition_with_engine(
    state: Arc<AppState>,
    project_conn: &rusqlite::Connection,
    engine_db_path: Option<String>,
    content: String,
    line: u32,
    character: u32,
    file_path: Option<String>,
) -> Result<Value> {
    let mut project_result = query::goto::goto_definition(
        project_conn,
        content.clone(),
        line,
        character,
        file_path.clone(),
    )?;

    if !project_result.is_null() {
        tag_value_source(&mut project_result, "project");
        return Ok(project_result);
    }

    let Some(engine_db_path) = engine_db_path else {
        return Ok(Value::Null);
    };

    let engine_db_path = normalize_to_native(&engine_db_path);
    if !Path::new(&engine_db_path).is_file() {
        return Ok(Value::Null);
    }

    let engine_conn = match state.get_read_only_connection(&engine_db_path) {
        Ok(conn) => conn,
        Err(err) => {
            warn!("Failed to open Engine DB for goto definition: {}", err);
            return Ok(Value::Null);
        }
    };

    let mut engine_result = match query::goto::goto_definition(
        &engine_conn,
        content,
        line,
        character,
        file_path,
    ) {
        Ok(value) => value,
        Err(err) => {
            warn!("Failed to query Engine DB goto definition: {}", err);
            return Ok(Value::Null);
        }
    };

    if !engine_result.is_null() {
        tag_value_source(&mut engine_result, "engine");
    }

    Ok(engine_result)
}

/// Go to implementation in the project DB, then fall back to the shared Engine DB.
/// 先在项目 DB 里跳转实现，找不到时回退到共享 Engine DB。
fn goto_implementation_with_engine(
    state: Arc<AppState>,
    project_conn: &rusqlite::Connection,
    engine_db_path: Option<String>,
    content: String,
    line: u32,
    character: u32,
    file_path: Option<String>,
) -> Result<Value> {
    let mut project_result = query::goto::goto_implementation(
        project_conn,
        content.clone(),
        line,
        character,
        file_path.clone(),
    )?;

    if !project_result.is_null() {
        tag_value_source(&mut project_result, "project");
        return Ok(project_result);
    }

    let Some(engine_db_path) = engine_db_path else {
        return Ok(Value::Null);
    };

    let engine_db_path = normalize_to_native(&engine_db_path);
    if !Path::new(&engine_db_path).is_file() {
        return Ok(Value::Null);
    }

    let engine_conn = match state.get_read_only_connection(&engine_db_path) {
        Ok(conn) => conn,
        Err(err) => {
            warn!("Failed to open Engine DB for goto implementation: {}", err);
            return Ok(Value::Null);
        }
    };

    let mut engine_result = match query::goto::goto_implementation(
        &engine_conn,
        content,
        line,
        character,
        file_path,
    ) {
        Ok(value) => value,
        Err(err) => {
            warn!("Failed to query Engine DB goto implementation: {}", err);
            return Ok(Value::Null);
        }
    };

    if !engine_result.is_null() {
        tag_value_source(&mut engine_result, "engine");
    }

    Ok(engine_result)
}

/// Resolve hover info in the project DB, then fall back to the shared Engine DB.
/// 先在项目 DB 里解析 hover，再回退到共享 Engine DB。
fn hover_with_engine(
    state: Arc<AppState>,
    project_conn: &rusqlite::Connection,
    engine_db_path: Option<String>,
    content: String,
    line: u32,
    character: u32,
    file_path: Option<String>,
) -> Result<Value> {
    let mut project_result = query::goto::get_hover(
        project_conn,
        content.clone(),
        line,
        character,
        file_path.clone(),
    )?;

    if !project_result.is_null() {
        tag_value_source(&mut project_result, "project");
        return Ok(project_result);
    }

    let Some(engine_db_path) = engine_db_path else {
        return Ok(Value::Null);
    };

    let engine_db_path = normalize_to_native(&engine_db_path);
    if !Path::new(&engine_db_path).is_file() {
        return Ok(Value::Null);
    }

    let engine_conn = match state.get_read_only_connection(&engine_db_path) {
        Ok(conn) => conn,
        Err(err) => {
            warn!("Failed to open Engine DB for hover: {}", err);
            return Ok(Value::Null);
        }
    };

    let mut engine_result = match query::goto::get_hover(
        &engine_conn,
        content,
        line,
        character,
        file_path,
    ) {
        Ok(value) => value,
        Err(err) => {
            warn!("Failed to query Engine DB hover: {}", err);
            return Ok(Value::Null);
        }
    };

    if !engine_result.is_null() {
        tag_value_source(&mut engine_result, "engine");
    }

    Ok(engine_result)
}

/// Resolve signature help in the project DB, then append Engine DB overloads.
/// 先在项目 DB 里解析签名帮助，再追加共享 Engine DB 的重载。
fn signature_help_with_engine(
    state: Arc<AppState>,
    project_conn: &rusqlite::Connection,
    engine_db_path: Option<String>,
    content: String,
    line: u32,
    character: u32,
    file_path: Option<String>,
) -> Result<Value> {
    let mut project_result = query::goto::get_signature_help(
        project_conn,
        content.clone(),
        line,
        character,
        file_path.clone(),
    )?;

    if project_result.is_null() {
        let Some(engine_db_path) = engine_db_path else {
            return Ok(Value::Null);
        };

        let engine_db_path = normalize_to_native(&engine_db_path);
        if !Path::new(&engine_db_path).is_file() {
            return Ok(Value::Null);
        }

        let engine_conn = match state.get_read_only_connection(&engine_db_path) {
            Ok(conn) => conn,
            Err(err) => {
                warn!("Failed to open Engine DB for signature help: {}", err);
                return Ok(Value::Null);
            }
        };

        let mut engine_result = match query::goto::get_signature_help(
            &engine_conn,
            content,
            line,
            character,
            file_path,
        ) {
            Ok(value) => value,
            Err(err) => {
                warn!("Failed to query Engine DB signature help: {}", err);
                return Ok(Value::Null);
            }
        };

        if !engine_result.is_null() {
            tag_value_source(&mut engine_result, "engine");
        }

        return Ok(engine_result);
    }

    if let Some(object) = project_result.as_object_mut() {
        if let Some(signatures) = object.get_mut("signatures").and_then(Value::as_array_mut) {
            tag_source(signatures, "project");
        }
    }

    let Some(engine_db_path) = engine_db_path else {
        return Ok(project_result);
    };

    let engine_db_path = normalize_to_native(&engine_db_path);
    if !Path::new(&engine_db_path).is_file() {
        return Ok(project_result);
    }

    let engine_conn = match state.get_read_only_connection(&engine_db_path) {
        Ok(conn) => conn,
        Err(err) => {
            warn!("Failed to open Engine DB for signature help: {}", err);
            return Ok(project_result);
        }
    };

    let engine_value = match query::goto::get_signature_help(
        &engine_conn,
        content,
        line,
        character,
        file_path,
    ) {
        Ok(value) => value,
        Err(err) => {
            warn!("Failed to query Engine DB signature help: {}", err);
            return Ok(project_result);
        }
    };

    if engine_value.is_null() {
        return Ok(project_result);
    }

    let Some(project_object) = project_result.as_object_mut() else {
        return Ok(project_result);
    };
    let Some(project_signatures) = project_object
        .get_mut("signatures")
        .and_then(Value::as_array_mut)
    else {
        return Ok(project_result);
    };

    let mut engine_signatures = engine_value
        .get("signatures")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    tag_source(&mut engine_signatures, "engine");
    merge_query_results(project_signatures, engine_signatures, 16);

    Ok(project_result)
}

/// Find references in the project DB, then merge matching Engine DB results.
/// 先查询项目 DB 的引用，再合并共享 Engine DB 的引用结果。
fn find_references_with_engine(
    state: Arc<AppState>,
    project_conn: &rusqlite::Connection,
    engine_db_path: Option<String>,
    symbol_name: &str,
    file_path: Option<&str>,
    content: Option<&str>,
    line: Option<u32>,
    character: Option<u32>,
) -> Result<Value> {
    let project_value = query::usage::find_symbol_usages_for_cursor(
        project_conn,
        symbol_name,
        file_path,
        content,
        line,
        character,
    )?;
    let mut results = nested_results_array(&project_value);
    tag_source(&mut results, "project");

    let mut searched_files = project_value
        .get("searched_files")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let mut found_definition = project_value
        .get("found_definition")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scope = project_value
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("global");

    // Project-local scopes should not merge Engine DB results; otherwise common member
    // names like CameraComponent get polluted by unrelated Engine symbols.
    // 项目内局部/成员作用域不合并 Engine DB，否则 CameraComponent 这类同名成员会被引擎源码污染。
    if matches!(scope, "local" | "member") {
        return Ok(json!({
            "results": results,
            "searched_files": searched_files,
            "found_definition": found_definition,
            "scope": scope,
        }));
    }

    let Some(engine_db_path) = engine_db_path else {
        return Ok(json!({
            "results": results,
            "searched_files": searched_files,
            "found_definition": found_definition,
            "scope": scope,
        }));
    };

    let engine_db_path = normalize_to_native(&engine_db_path);
    if !Path::new(&engine_db_path).is_file() {
        return Ok(json!({
            "results": results,
            "searched_files": searched_files,
            "found_definition": found_definition,
            "scope": scope,
        }));
    }

    let engine_conn = match state.get_read_only_connection(&engine_db_path) {
        Ok(conn) => conn,
        Err(err) => {
            warn!("Failed to open Engine DB for references: {}", err);
            return Ok(json!({
                "results": results,
                "searched_files": searched_files,
                "found_definition": found_definition,
                "scope": scope,
            }));
        }
    };

    match query::usage::find_symbol_usages_for_cursor(
        &engine_conn,
        symbol_name,
        file_path,
        content,
        line,
        character,
    ) {
        Ok(engine_value) => {
            searched_files += engine_value
                .get("searched_files")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            found_definition = found_definition
                || engine_value
                    .get("found_definition")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

            let mut engine_results = nested_results_array(&engine_value);
            tag_source(&mut engine_results, "engine");
            merge_query_results(&mut results, engine_results, 300);
        }
        Err(err) => {
            warn!("Failed to query Engine DB references: {}", err);
        }
    }

    Ok(json!({
        "results": results,
        "searched_files": searched_files,
        "found_definition": found_definition,
        "scope": scope,
    }))
}

/// Add a source marker to one query result object.
/// 给单个查询结果对象添加来源标记。
fn tag_value_source(value: &mut Value, source: &str) {
    if let Some(object) = value.as_object_mut() {
        object.entry("source").or_insert_with(|| json!(source));
    }
}

/// Search symbols in the project DB, then merge matching Engine DB results.
/// 先查询项目 DB，再合并 Engine DB 的符号搜索结果。
fn search_symbols_with_engine(
    state: Arc<AppState>,
    project_conn: &rusqlite::Connection,
    engine_db_path: Option<String>,
    pattern: &str,
    limit: usize,
    offset: usize,
) -> Result<Value> {
    let limit = limit.clamp(1, 10_000);
    let mut results =
        value_array(query::search::search_symbols(project_conn, pattern, limit, offset)?);
    tag_source(&mut results, "project");

    if results.len() >= limit {
        results.truncate(limit);
        return Ok(json!(results));
    }

    let Some(engine_db_path) = engine_db_path else {
        return Ok(json!(results));
    };

    let engine_db_path = normalize_to_native(&engine_db_path);
    if !Path::new(&engine_db_path).is_file() {
        return Ok(json!(results));
    }

    let engine_conn = match state.get_read_only_connection(&engine_db_path) {
        Ok(conn) => conn,
        Err(err) => {
            warn!("Failed to open Engine DB for symbol search: {}", err);
            return Ok(json!(results));
        }
    };

    let remaining = limit.saturating_sub(results.len()).max(1);
    let mut engine_results =
        match query::search::search_symbols(&engine_conn, pattern, remaining, 0) {
            Ok(value) => value_array(value),
            Err(err) => {
                warn!("Failed to query Engine DB symbols: {}", err);
                return Ok(json!(results));
            }
        };

    tag_source(&mut engine_results, "engine");
    merge_query_results(&mut results, engine_results, limit);

    Ok(json!(results))
}

fn global_find_with_engine(
    state: Arc<AppState>,
    project_conn: &rusqlite::Connection,
    engine_db_path: Option<String>,
    pattern: &str,
    limit: usize,
    offset: usize,
) -> Result<Value> {
    let limit = limit.clamp(1, 10_000);
    let mut results = value_array(query::search::global_find(project_conn, pattern, limit, offset)?);
    tag_source(&mut results, "project");

    if results.len() >= limit {
        return Ok(Value::Array(results));
    }

    let Some(engine_db_path) = engine_db_path.filter(|path| !path.trim().is_empty()) else {
        return Ok(Value::Array(results));
    };

    let engine_db_path = normalize_to_native(&engine_db_path);
    if !Path::new(&engine_db_path).is_file() {
        return Ok(Value::Array(results));
    }

    let engine_conn = match state.get_read_only_connection(&engine_db_path) {
        Ok(conn) => conn,
        Err(err) => {
            warn!("Failed to open Engine DB for global find: {}", err);
            return Ok(Value::Array(results));
        }
    };
    let remaining = limit.saturating_sub(results.len()).max(1);
    let mut engine_results = match query::search::global_find(&engine_conn, pattern, remaining, 0) {
        Ok(value) => value_array(value),
        Err(err) => {
            warn!("Failed to query Engine DB global find: {}", err);
            Vec::new()
        }
    };

    tag_source(&mut engine_results, "engine");
    results.extend(engine_results);
    results.truncate(limit);

    Ok(Value::Array(results))
}

fn fast_find_with_scope(
    state: Arc<AppState>,
    project_conn: &rusqlite::Connection,
    engine_db_path: Option<String>,
    pattern: &str,
    limit: usize,
    offset: usize,
    scope: Option<&str>,
) -> Result<Value> {
    let scope = scope.unwrap_or("both");
    if scope == "engine" {
        let Some(engine_conn) = open_engine_query_connection(state, engine_db_path, "fast find")? else {
            return Ok(json!([]));
        };
        let mut results = value_array(query::search::fast_find(&engine_conn, pattern, limit, offset)?);
        tag_source(&mut results, "engine");
        return Ok(json!(results));
    }

    let mut results = value_array(query::search::fast_find(project_conn, pattern, limit, offset)?);
    tag_source(&mut results, "project");

    if scope == "project" || results.len() >= limit {
        results.truncate(limit);
        return Ok(json!(results));
    }

    let Some(engine_conn) = open_engine_query_connection(state, engine_db_path, "fast find")? else {
        return Ok(json!(results));
    };
    let remaining = limit.saturating_sub(results.len()).max(1);
    let mut engine_results = value_array(query::search::fast_find(&engine_conn, pattern, remaining, 0)?);
    tag_source(&mut engine_results, "engine");
    merge_query_results(&mut results, engine_results, limit);

    Ok(json!(results))
}

fn search_code_text_with_scope(
    state: Arc<AppState>,
    project_conn: &rusqlite::Connection,
    engine_db_path: Option<String>,
    pattern: &str,
    limit: usize,
    offset: usize,
    scope: Option<&str>,
) -> Result<Value> {
    let scope = scope.unwrap_or("project");
    if scope == "engine" {
        let Some(engine_conn) = open_engine_query_connection(state, engine_db_path, "code text")? else {
            return Ok(json!([]));
        };
        let mut results = value_array(query::search::search_code_text(&engine_conn, pattern, limit, offset)?);
        tag_source(&mut results, "engine");
        return Ok(json!(results));
    }

    let mut results = value_array(query::search::search_code_text(project_conn, pattern, limit, offset)?);
    tag_source(&mut results, "project");
    Ok(json!(results))
}

fn open_engine_query_connection(
    state: Arc<AppState>,
    engine_db_path: Option<String>,
    label: &str,
) -> Result<Option<rusqlite::Connection>> {
    let Some(engine_db_path) = engine_db_path.filter(|path| !path.trim().is_empty()) else {
        return Ok(None);
    };

    let engine_db_path = normalize_to_native(&engine_db_path);
    if !Path::new(&engine_db_path).is_file() {
        return Ok(None);
    }

    match state.get_read_only_connection(&engine_db_path) {
        Ok(conn) => Ok(Some(conn)),
        Err(err) => {
            warn!("Failed to open Engine DB for {}: {}", label, err);
            Ok(None)
        }
    }
}

/// Convert a JSON array value into a Vec.
/// 将 JSON array value 转成 Vec。
fn value_array(value: Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_default()
}

/// Extract the `results` array from an object response.
/// 从对象响应里提取 `results` 数组。
fn nested_results_array(value: &Value) -> Vec<Value> {
    value
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Add a source marker to query result objects.
/// 给查询结果对象添加来源标记。
fn tag_source(items: &mut [Value], source: &str) {
    for item in items {
        if let Some(object) = item.as_object_mut() {
            object.entry("source").or_insert_with(|| json!(source));
        }
    }
}

/// Merge query results while keeping project results first and avoiding duplicates.
/// 合并查询结果，保持项目结果优先，并去重。
fn merge_query_results(target: &mut Vec<Value>, extra: Vec<Value>, limit: usize) {
    let mut seen = target
        .iter()
        .map(result_identity)
        .collect::<HashSet<String>>();

    for item in extra {
        if target.len() >= limit {
            break;
        }

        let identity = result_identity(&item);
        if seen.insert(identity) {
            target.push(item);
        }
    }
}

/// Build a stable identity for de-duplicating merged query results.
/// 为合并查询结果构造稳定去重 key。
fn result_identity(item: &Value) -> String {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let path = item
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let line = item
        .get("line")
        .and_then(Value::as_i64)
        .unwrap_or_default();

    format!("{}:{}:{}", name, path, line)
}

// -----------------------------------------------------------------------------
// Scan handler
// -----------------------------------------------------------------------------

/// Scan a batch of files and save parsed symbols into SQLite.
/// 扫描一批文件，并把解析结果保存到 SQLite。
pub async fn handle_scan(state: &AppState, params: &Value) -> Result<Value> {
    let req: ScanRequest = convert_params(params)?;

    let db_path = req
        .files
        .first()
        .and_then(|file| file.db_path.clone())
        .ok_or_else(|| anyhow!("No DB path"))?;

    let db_path_native = normalize_to_native(&db_path);
    let conn = state.get_connection(&db_path_native)?;

    tokio::task::spawn_blocking(move || {
        let language = tree_sitter_unreal_cpp::LANGUAGE.into();
        let query = tree_sitter::Query::new(&language, scanner::QUERY_STR)?;
        let include_query = tree_sitter::Query::new(&language, scanner::INCLUDE_QUERY_STR)?;

        let results = req
            .files
            .into_iter()
            .filter_map(|input| {
                scanner::process_file(&input, &language, &query, &include_query).ok()
            })
            .collect::<Vec<_>>();

        let mut conn = conn.lock();
        db::save_to_db(&mut conn, &results, Arc::new(crate::types::StdoutReporter))?;

        Ok(json!(results.len()))
    })
    .await?
}

// -----------------------------------------------------------------------------
// Status handlers
// -----------------------------------------------------------------------------

/// Return server status.
/// 获取 server 当前状态。
pub async fn get_status(state: &AppState) -> Result<Value> {
    let projects = state.projects.lock();
    let clients = state.active_clients.lock();
    let exe_path = std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().to_string());
    let server_version = env!("CARGO_PKG_VERSION");

    Ok(json!({
        "status": "running",
        "protocol_version": SERVER_PROTOCOL_VERSION,
        "server_version": server_version,
        "build_id": format!("{}-p{}", server_version, SERVER_PROTOCOL_VERSION),
        "exe_path": exe_path,
        "active_projects": projects.keys().cloned().collect::<Vec<_>>(),
        "active_clients": clients.iter().copied().collect::<Vec<_>>(),
    }))
}

/// List registered projects.
/// 列出已注册工程。
pub async fn list_projects(state: &AppState) -> Result<Value> {
    let projects = state.projects.lock();

    let list = projects
        .iter()
        .map(|(root, ctx)| {
            json!({
                "root": root,
                "db_path": ctx.db_path,
                "cache_db_path": ctx.cache_db_path,
                "vcs_hash": ctx.vcs_hash,
            })
        })
        .collect::<Vec<_>>();

    Ok(json!(list))
}

// -----------------------------------------------------------------------------
// Modify handlers
// -----------------------------------------------------------------------------

/// Add a module to a .uproject or .uplugin file.
/// 给 .uproject 或 .uplugin 添加模块。
pub async fn handle_modify_uproject_add_module(params: &Value) -> Result<Value> {
    let req: ModifyUprojectAddModuleRequest = convert_params(params)?;

    let result = tokio::task::spawn_blocking(move || {
        crate::edit::uproject::add_module(
            &req.file_path,
            &req.module_name,
            &req.module_type,
            &req.loading_phase,
        )
    })
    .await?;

    Ok(modify_result_to_json(result))
}

/// Add a module to a .Target.cs file.
/// 给 .Target.cs 添加模块。
pub async fn handle_modify_target_add_module(params: &Value) -> Result<Value> {
    let req: ModifyTargetAddModuleRequest = convert_params(params)?;

    let result = tokio::task::spawn_blocking(move || {
        crate::edit::target::add_module(&req.file_path, &req.module_name)
    })
    .await?;

    Ok(modify_result_to_json(result))
}

/// Convert modify result into public JSON shape.
/// 把修改结果转换成公开 JSON 结构。
fn modify_result_to_json(result: Result<()>) -> Value {
    match result {
        Ok(()) => serde_json::to_value(ModifyResult {
            success: true,
            message: None,
        })
        .unwrap_or_else(|_| json!({ "success": true })),

        Err(err) => serde_json::to_value(ModifyResult {
            success: false,
            message: Some(err.to_string()),
        })
        .unwrap_or_else(|_| json!({ "success": false, "message": err.to_string() })),
    }
}

// -----------------------------------------------------------------------------
// Asset query helpers
// -----------------------------------------------------------------------------

/// Get asset usages from in-memory AssetGraph.
/// 从内存 AssetGraph 查询资产引用和派生关系。
fn get_asset_usages(state: &AppState, root_key: &str, asset_path: &str) -> Value {
    if is_asset_scan_active(state, root_key) {
        return json!({
            "status": "scanning",
            "references": [],
            "derived": [],
        });
    }

    let graphs = state.asset_graphs.lock();
    let Some(graph) = graphs.get(root_key) else {
        return json!({
            "status": "scanning",
            "references": [],
            "derived": [],
        });
    };

    let try_names = make_asset_lookup_names(asset_path);
    let mut references = HashSet::new();
    let mut derived = HashSet::new();

    for name in &try_names {
        let dot_name = format!(".{}", name);

        for (key, assets) in &graph.references {
            if key.as_ref() == name || key.ends_with(&dot_name) {
                references.extend(assets.iter().map(|item| item.to_string()));
            }
        }

        for (key, assets) in &graph.derived {
            if key.as_ref() == name || key.ends_with(&dot_name) {
                derived.extend(assets.iter().map(|item| item.to_string()));
            }
        }

        for (key, assets) in &graph.functions {
            if key.as_ref() == name || key.ends_with(&dot_name) || key.contains(&format!(":{}", name))
            {
                references.extend(assets.iter().map(|item| item.to_string()));
            }
        }
    }

    json!({
        "status": "ready",
        "references": sorted_strings(references),
        "derived": sorted_strings(derived),
    })
}

/// Get dependencies for a single asset by parsing the asset file directly.
/// 直接解析单个资产文件，获取它依赖的资源和父类。
fn get_asset_dependencies(project_root: &str, asset_path: &str) -> Result<Value> {
    if asset_path.starts_with("/Script/") {
        return Ok(json!({
            "dependencies": [],
            "parent_class": Value::Null,
        }));
    }

    let Some(file_path) = find_asset_file(project_root, asset_path) else {
        return Ok(json!({
            "dependencies": [],
            "parent_class": Value::Null,
        }));
    };

    let mut parser = crate::uasset::UAssetParser::new();
    parser.parse(&file_path)?;

    let mut deps = parser.imports;
    deps.sort();
    deps.dedup();

    Ok(json!({
        "dependencies": deps,
        "parent_class": parser.parent_class,
    }))
}

/// Merge Blueprint-derived assets into class query result.
/// 把蓝图派生资产合并进 class 派生查询结果。
fn merge_asset_derived_classes(
    state: &AppState,
    root_key: &str,
    base_class: &str,
    results: &mut Vec<Value>,
) {
    if is_asset_scan_active(state, root_key) {
        results.push(json!({
            "name": "Scanning...",
            "path": "",
            "symbol_type": "scanning",
        }));
        return;
    }

    let graphs = state.asset_graphs.lock();
    let Some(graph) = graphs.get(root_key) else {
        return;
    };

    let names = make_asset_lookup_names(base_class);

    for name in names {
        let dot_name = format!(".{}", name);

        for (key, assets) in &graph.derived {
            if key.as_ref() != name && !key.ends_with(&dot_name) {
                continue;
            }

            for asset in assets {
                let exists = results.iter().any(|item| {
                    item["path"]
                        .as_str()
                        .map(|path| path.eq_ignore_ascii_case(asset.as_ref()))
                        .unwrap_or(false)
                });

                if !exists {
                    results.push(json!({
                        "name": asset.rsplit('/').next().unwrap_or(asset.as_ref()),
                        "path": asset.to_string(),
                        "symbol_type": "uasset",
                    }));
                }
            }
        }
    }
}

/// Get all assets known by the in-memory graph.
/// 从内存资产图获取所有已知资产。
fn get_assets_from_graph(state: &AppState, root_key: &str) -> Value {
    let graphs = state.asset_graphs.lock();
    let Some(graph) = graphs.get(root_key) else {
        return json!([]);
    };

    let mut assets = HashSet::new();

    for values in graph.references.values() {
        assets.extend(values.iter().map(|item| item.to_string()));
    }

    for values in graph.derived.values() {
        assets.extend(values.iter().map(|item| item.to_string()));
    }

    json!(sorted_strings(assets))
}

// -----------------------------------------------------------------------------
// Shared helpers
// -----------------------------------------------------------------------------

/// Ensure SQLite database exists, version matches, and has required data.
/// 确保 SQLite 数据库存在、版本正确，并且不是空索引。
async fn ensure_database_ready(db_path_native: String) -> Result<bool> {
    tokio::task::spawn_blocking(move || {
        let reinitialized = db::ensure_correct_version(&db_path_native).unwrap_or(false);

        if reinitialized {
            return Ok(true);
        }

        let conn = rusqlite::Connection::open(&db_path_native)?;
        let file_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .unwrap_or(0);
        let class_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM classes", [], |row| row.get(0))
            .unwrap_or(0);

        Ok(file_count == 0 || class_count == 0)
    })
    .await?
}

/// Remove cached database connections before refresh/setup.
/// setup 或 refresh 前移除旧 DB 连接缓存。
fn drop_db_connections(state: &AppState, db_path_native: &str, cache_db_path_unix: Option<&str>) {
    let mut conns = state.connections.lock();
    conns.remove(db_path_native);
    drop(conns);

    if let Some(cache_path) = cache_db_path_unix {
        let mut cache_conns = state.persistent_cache_connections.lock();
        cache_conns.remove(&normalize_to_native(cache_path));
    }
}

/// Insert or update project context for refresh.
/// refresh 前插入或更新工程上下文。
fn upsert_refresh_project_context(
    state: &AppState,
    req: &mut RefreshRequest,
    root_key: &str,
) -> Result<String> {
    let mut projects = state.projects.lock();

    if let Some(db_path) = &req.db_path {
        let db_path_unix = normalize_to_unix(db_path);

        projects.insert(
            root_key.to_string(),
            ProjectContext {
                db_path: db_path_unix.clone(),
                cache_db_path: req.cache_db_path.as_ref().map(|p| normalize_to_unix(p)),
                vcs_hash: req.vcs_hash.clone(),
                last_refresh_at: Instant::now(),
            },
        );

        return Ok(db_path_unix);
    }

    let Some(ctx) = projects.get_mut(root_key) else {
        return Err(anyhow!("Project not found: {}", root_key));
    };

    ctx.vcs_hash = req.vcs_hash.clone();

    if let Some(cache_path) = &req.cache_db_path {
        ctx.cache_db_path = Some(normalize_to_unix(cache_path));
    }

    Ok(ctx.db_path.clone())
}

/// Clear completion cache after project refresh.
/// 工程刷新后清空补全缓存。
fn clear_completion_cache(state: &AppState, root_key: &str) {
    let cache = state.get_completion_cache(root_key);
    cache.lock().clear();
    info!("Completion cache cleared after refresh: {}", root_key);
}

/// Check whether a refresh is active for the project.
/// 判断工程是否正在 refresh。
fn is_refreshing(state: &AppState, root_key: &str) -> bool {
    state.active_refreshes.lock().contains(root_key)
}

/// Check whether asset scan is active.
/// 判断资产扫描是否正在进行。
fn is_asset_scan_active(state: &AppState, root_key: &str) -> bool {
    state.active_asset_scans.lock().contains(root_key)
}

/// Start asset scan if graph is missing and no scan is active.
/// 如果缺少资产图且当前没在扫描，则启动资产扫描。
fn ensure_asset_scan_started(state: &Arc<AppState>, root_key: &str, project_root: &str) {
    let has_graph = state.asset_graphs.lock().contains_key(root_key);
    if has_graph {
        return;
    }

    let mut active = state.active_asset_scans.lock();
    if !active.insert(root_key.to_string()) {
        return;
    }

    drop(active);

    let state_clone = state.clone();
    let project_root = project_root.to_string();

    tokio::spawn(async move {
        handle_asset_scan(state_clone, project_root).await;
    });
}

/// Setup path also starts asset scanning after project registration.
/// setup 成功注册工程后也启动资产扫描。
fn spawn_asset_scan_if_needed(state: Arc<AppState>, root_key: String, project_root: String) {
    let mut active = state.active_asset_scans.lock();

    if !active.insert(root_key) {
        return;
    }

    drop(active);

    tokio::spawn(async move {
        handle_asset_scan(state, project_root).await;
    });
}

/// Guard for active_refreshes.
/// active_refreshes 的自动清理保护对象。
struct RefreshGuard<'a> {
    state: &'a AppState,
    root_key: String,
}

impl<'a> RefreshGuard<'a> {
    /// Create guard or return early if refresh is already active.
    /// 创建 refresh guard；如果已经在刷新则直接返回错误。
    fn try_new(state: &'a AppState, root_key: String) -> Result<Self> {
        let mut active = state.active_refreshes.lock();

        if !active.insert(root_key.clone()) {
            return Err(anyhow!("Refresh already in progress"));
        }

        Ok(Self { state, root_key })
    }
}

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.state.active_refreshes.lock().remove(&self.root_key);
    }
}

/// Get registered project context by root key.
/// 根据 root_key 获取工程上下文。
fn get_project_context(state: &AppState, root_key: &str) -> Result<ProjectContext> {
    let projects = state.projects.lock();

    projects
        .get(root_key)
        .cloned()
        .ok_or_else(|| anyhow!("Project not found: {}", root_key))
}

/// Build lookup names for class or asset references.
/// 为 class 或 asset 引用构造多个可匹配名称。
fn make_asset_lookup_names(input: &str) -> Vec<String> {
    let class_name = if input.starts_with("/Script/") {
        input.rsplit('.').next().unwrap_or(input)
    } else {
        input
    };

    let mut names = vec![class_name.to_ascii_lowercase()];

    let prefixes = ['a', 'u', 'f', 'e', 't', 's'];
    let mut chars = class_name.chars();

    if let (Some(first), Some(second)) = (chars.next(), chars.next()) {
        if prefixes.contains(&first.to_ascii_lowercase()) && second.is_uppercase() {
            names.push(class_name[first.len_utf8()..].to_ascii_lowercase());
        }
    }

    names.sort();
    names.dedup();
    names
}

/// Locate an asset file from an Unreal /Game path.
/// 根据 Unreal /Game 路径定位真实资产文件。
fn find_asset_file(project_root: &str, asset_path: &str) -> Option<PathBuf> {
    let root = PathBuf::from(normalize_to_native(project_root));
    let relative = asset_path.replacen("/Game/", "Content/", 1);
    let basename = relative.rsplit('/').next().unwrap_or("");

    let candidates = [
        format!("{}.uasset", basename),
        format!("{}.umap", basename),
    ];

    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(name.as_ref(), "Intermediate" | "Binaries" | "Build" | "Saved")
        })
        .build();

    for entry in walker.filter_map(|entry| entry.ok()) {
        let name = entry.file_name().to_string_lossy();

        if !candidates.iter().any(|candidate| candidate == name.as_ref()) {
            continue;
        }

        let normalized = entry.path().to_string_lossy().replace('\\', "/");

        if normalized.contains(&relative) {
            return Some(entry.path().to_path_buf());
        }
    }

    None
}

/// Sort a string set into deterministic order.
/// 把字符串集合排序成稳定输出。
fn sorted_strings(values: HashSet<String>) -> Vec<String> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort();
    values
}
