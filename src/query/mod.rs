use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde_json::Value;

use crate::types::QueryRequest;

pub mod asset;
pub mod buffer;
pub mod class;
pub mod config;
pub mod file;
pub mod goto;
pub mod module;
pub mod search;
pub mod usage;

/// Process a normal query and return one JSON value.
/// 处理普通查询，并一次性返回一个 JSON 结果。
pub fn process_query(conn: &Connection, request: QueryRequest) -> Result<Value> {
    match request {
        // ---------------------------------------------------------------------
        // File queries
        // 文件查询
        // ---------------------------------------------------------------------

        QueryRequest::GetFilesInModules {
            modules,
            extensions,
            filter,
        } => file::get_files_in_modules(conn, modules, extensions, filter),

        QueryRequest::GetDependFiles {
            file_path,
            recursive,
            game_only,
        } => file::get_depend_files(conn, &file_path, recursive, game_only),

        QueryRequest::SearchFiles { part }
        | QueryRequest::SearchFilesByPathPart { part } => {
            file::search_files_by_path_part(conn, &part)
        }

        QueryRequest::GetTargetFiles => file::get_target_files(conn),

        QueryRequest::GetAllFilePaths => file::get_all_file_paths(conn),

        QueryRequest::GetAllFilesMetadata => file::get_all_files_metadata(conn),

        // ---------------------------------------------------------------------
        // Symbol search queries
        // 符号搜索查询
        // ---------------------------------------------------------------------

        QueryRequest::SearchSymbols {
            pattern,
            limit,
            offset,
        } => {
            search::search_symbols(conn, &pattern, limit, offset)
        }
        QueryRequest::FastFind {
            pattern,
            limit,
            offset,
            ..
        } => search::fast_find(conn, &pattern, limit, offset),
        QueryRequest::SearchCodeText {
            pattern,
            limit,
            offset,
            ..
        } => search::search_code_text(conn, &pattern, limit, offset),
        QueryRequest::GlobalFind {
            pattern,
            limit,
            offset,
        } => search::global_find(conn, &pattern, limit, offset),

        QueryRequest::GetStructsOnly => search::get_structs(conn),

        // ---------------------------------------------------------------------
        // Class/member queries
        // 类和成员查询
        // ---------------------------------------------------------------------

        QueryRequest::GetFileSymbols { file_path } => {
            class::get_file_symbols(conn, &file_path)
        }

        QueryRequest::GetClassMembers { class_name } => {
            class::get_class_members(conn, &class_name)
        }

        QueryRequest::SearchClassesPrefix { prefix, limit } => {
            class::search_classes_prefix(conn, &prefix, limit)
        }

        QueryRequest::GetClassesInModules {
            modules,
            symbol_type,
        } => class::get_classes_in_modules(conn, modules, symbol_type),

        QueryRequest::GetEnumValues { enum_name } => {
            class::get_enum_values(conn, &enum_name)
        }

        // This branch is only needed if query/util.rs is kept.
        // 只有你保留 query/util.rs 时才需要这个分支。
        QueryRequest::GetClassFilePath { .. } => {
            Err(anyhow!("GetClassFilePath is not implemented"))
        }

        // ---------------------------------------------------------------------
        // Module queries
        // 模块查询
        // ---------------------------------------------------------------------

        QueryRequest::GetModules => module::get_modules(conn),

        QueryRequest::GetModuleByName { name } => {
            module::get_module_by_name(conn, &name)
        }

        // ---------------------------------------------------------------------
        // Buffer queries
        // 当前 buffer 查询
        // ---------------------------------------------------------------------

        QueryRequest::ParseBuffer {
            content,
            file_path,
            line,
            character,
        } => buffer::parse_buffer(content, file_path, line, character),

        // ---------------------------------------------------------------------
        // Navigation queries
        // 跳转查询
        // ---------------------------------------------------------------------

        QueryRequest::GotoDefinition {
            content,
            line,
            character,
            file_path,
        } => goto::goto_definition(conn, content, line, character, file_path),

        QueryRequest::GotoImplementation {
            content,
            line,
            character,
            file_path,
        } => goto::goto_implementation(conn, content, line, character, file_path),
        QueryRequest::GetHover {
            content,
            line,
            character,
            file_path,
        } => goto::get_hover(conn, content, line, character, file_path),
        QueryRequest::GetSignatureHelp {
            content,
            line,
            character,
            file_path,
        } => goto::get_signature_help(conn, content, line, character, file_path),

        QueryRequest::FindSymbolInInheritanceChain {
            class_name,
            symbol_name,
            ..
        } => Ok(goto::find_symbol_in_inheritance_chain(conn, &class_name, &symbol_name)?
            .unwrap_or(Value::Null)),

        QueryRequest::FindSymbolInModule { module, symbol } => {
            Ok(goto::find_symbol_in_module(conn, &module, &symbol)?
                .unwrap_or(Value::Null))
        }

        // ---------------------------------------------------------------------
        // Usage queries
        // 引用查询
        // ---------------------------------------------------------------------

        QueryRequest::FindSymbolUsages {
            symbol_name,
            file_path,
            content: _,
            line: _,
            character: _,
        } => usage::find_symbol_usages(conn, &symbol_name, file_path.as_deref()),

        // ---------------------------------------------------------------------
        // Asset/component queries
        // 资源和组件查询
        // ---------------------------------------------------------------------

        QueryRequest::GetAssets => asset::get_assets(conn),

        QueryRequest::GrepAssets { pattern } => {
            asset::grep_assets(conn, pattern, |_| Ok(()))
        }

        QueryRequest::GetComponents => crate::db::get_components(conn),

        // ---------------------------------------------------------------------
        // Server-state-only queries
        // 只能由 server state 处理的查询
        // ---------------------------------------------------------------------

        QueryRequest::GetConfigData { .. } => {
            Err(anyhow!("GetConfigData must be handled by server state"))
        }

        // ---------------------------------------------------------------------
        // Fallback
        // 兜底
        // ---------------------------------------------------------------------

        other => Err(anyhow!(
            "Query type not implemented in query dispatcher: {:?}",
            other
        )),
    }
}

/// Process a streaming query.
/// 处理流式查询，用 on_items 分批返回结果。
pub fn process_query_streaming<F>(
    conn: &Connection,
    request: QueryRequest,
    on_items: F,
) -> Result<Value>
where
    F: FnMut(Vec<Value>) -> Result<()>,
{
    match request {
        // ---------------------------------------------------------------------
        // Asset streaming
        // 资源流式查询
        // ---------------------------------------------------------------------

        QueryRequest::GrepAssets { pattern } => {
            asset::grep_assets(conn, pattern, on_items)
        }

        // ---------------------------------------------------------------------
        // File streaming
        // 文件流式查询
        // ---------------------------------------------------------------------

        QueryRequest::GetFilesInModulesAsync {
            modules,
            extensions,
            filter,
        } => file::get_files_in_modules_async(
            conn,
            modules,
            extensions,
            filter,
            on_items,
        ),

        QueryRequest::SearchFilesInModulesAsync {
            modules,
            filter,
            limit,
        } => asset::search_files_in_modules_async(
            conn,
            modules,
            filter,
            limit,
            on_items,
        ),

        QueryRequest::SearchFilesByPathPartAsync { part } => {
            file::search_files_by_path_part_async(conn, &part, on_items)
        }

        // ---------------------------------------------------------------------
        // Class streaming
        // 类流式查询
        // ---------------------------------------------------------------------

        QueryRequest::GetClassesInModulesAsync {
            modules,
            symbol_type,
        } => class::get_classes_in_modules_async(
            conn,
            modules,
            symbol_type,
            on_items,
        ),

        // ---------------------------------------------------------------------
        // Usage streaming
        // 引用流式查询
        // ---------------------------------------------------------------------

        QueryRequest::FindSymbolUsagesAsync {
            symbol_name,
            file_path,
        } => usage::find_symbol_usages_async(
            conn,
            &symbol_name,
            file_path.as_deref(),
            on_items,
        ),

        // ---------------------------------------------------------------------
        // Non-streaming fallback
        // 非流式请求兜底走普通查询。
        // ---------------------------------------------------------------------

        other => process_query(conn, other),
    }
}
