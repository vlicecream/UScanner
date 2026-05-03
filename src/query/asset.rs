use std::fs::File;

use anyhow::Result;
use memmap2::Mmap;
use rayon::prelude::*;
use rusqlite::{params_from_iter, Connection, ToSql};
use serde_json::{json, Value};

use crate::db::project_path::PATH_CTE;

const DEFAULT_FILE_LIMIT: usize = 100;
const DEFAULT_ASYNC_FILE_LIMIT: usize = 1000;
const MODULE_CHUNK_SIZE: usize = 500;
const ASYNC_BATCH_SIZE: usize = 200;
const ASSET_BATCH_SIZE: usize = 500;

/// Search binary Unreal asset files for a byte pattern.
/// 在 Unreal 二进制资源文件中搜索字节模式。
pub fn grep_assets<F>(
    conn: &Connection,
    pattern: String,
    mut on_items: F,
) -> Result<Value>
where
    F: FnMut(Vec<Value>) -> Result<()>,
{
    if pattern.is_empty() {
        return Ok(json!(0));
    }

    let file_paths = collect_asset_paths(conn, true)?;
    let pattern_bytes = pattern.into_bytes();

    let matched_paths: Vec<String> = file_paths
        .par_iter()
        .filter(|path| file_contains_bytes(path, &pattern_bytes))
        .cloned()
        .collect();

    for chunk in matched_paths.chunks(ASSET_BATCH_SIZE) {
        let items = chunk.iter().map(|path| json!(path)).collect();
        on_items(items)?;
    }

    Ok(json!(matched_paths.len()))
}

/// Return indexed Unreal asset files.
/// 返回已索引的 Unreal 资源文件。
pub fn get_assets(conn: &Connection) -> Result<Value> {
    let sql = format!(
        r#"
        {}
        SELECT
            CASE
                WHEN dp.full_path = '/' THEN '/' || sn.text
                WHEN substr(dp.full_path, -1) = '/' THEN dp.full_path || sn.text
                ELSE dp.full_path || '/' || sn.text
            END AS path,
            sn.text AS filename
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        WHERE LOWER(f.extension) IN ('uasset', 'umap')
        ORDER BY path
        LIMIT 1000
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "path": row.get::<_, String>(0)?,
            "filename": row.get::<_, String>(1)?,
        }))
    })?;

    collect_json_rows(rows)
}

/// Search files by filename.
/// 按文件名搜索文件。
pub fn search_files(conn: &Connection, part: String) -> Result<Value> {
    let sql = format!(
        r#"
        {}
        SELECT
            CASE
                WHEN dp.full_path = '/' THEN '/' || sn.text
                WHEN substr(dp.full_path, -1) = '/' THEN dp.full_path || sn.text
                ELSE dp.full_path || '/' || sn.text
            END AS path,
            sn.text AS filename
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        WHERE sn.text LIKE ?1 ESCAPE '\'
        ORDER BY sn.text
        LIMIT ?2
        "#,
        PATH_CTE
    );

    let pattern = like_contains_pattern(&part);
    let limit = DEFAULT_FILE_LIMIT as i64;

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map((&pattern, limit), |row| {
        Ok(json!({
            "path": row.get::<_, String>(0)?,
            "filename": row.get::<_, String>(1)?,
        }))
    })?;

    collect_json_rows(rows)
}

/// Search files inside selected modules and return all results at once.
/// 在指定模块中搜索文件，并一次性返回所有结果。
pub fn search_files_in_modules(
    conn: &Connection,
    modules: Vec<String>,
    filter: String,
    limit: Option<usize>,
) -> Result<Value> {
    if modules.is_empty() {
        return Ok(json!([]));
    }

    let limit = limit.unwrap_or(DEFAULT_FILE_LIMIT);
    let mut all_files = Vec::new();

    for chunk in modules.chunks(MODULE_CHUNK_SIZE) {
        if all_files.len() >= limit {
            break;
        }

        let remaining = limit - all_files.len();
        let rows = query_files_in_module_chunk(conn, chunk, &filter, remaining)?;

        for row in rows {
            all_files.push(row);

            if all_files.len() >= limit {
                break;
            }
        }
    }

    Ok(json!(all_files))
}

/// Search files inside selected modules and stream result batches.
/// 在指定模块中搜索文件，并分批回调返回结果。
pub fn search_files_in_modules_async<F>(
    conn: &Connection,
    modules: Vec<String>,
    filter: String,
    limit: Option<usize>,
    mut on_items: F,
) -> Result<Value>
where
    F: FnMut(Vec<Value>) -> Result<()>,
{
    if modules.is_empty() {
        return Ok(json!(0));
    }

    let limit = limit.unwrap_or(DEFAULT_ASYNC_FILE_LIMIT);
    let mut total_sent = 0usize;

    for chunk in modules.chunks(MODULE_CHUNK_SIZE) {
        if total_sent >= limit {
            break;
        }

        let remaining = limit - total_sent;
        let rows = query_files_in_module_chunk(conn, chunk, &filter, remaining)?;
        let mut batch = Vec::with_capacity(ASYNC_BATCH_SIZE);

        for row in rows {
            if total_sent + batch.len() >= limit {
                break;
            }

            batch.push(row);

            if batch.len() >= ASYNC_BATCH_SIZE {
                total_sent += batch.len();
                on_items(std::mem::take(&mut batch))?;
            }
        }

        if !batch.is_empty() {
            total_sent += batch.len();
            on_items(batch)?;
        }
    }

    Ok(json!(total_sent))
}

/// Collect asset paths from the index.
/// 从索引中收集资源路径。
fn collect_asset_paths(conn: &Connection, content_only: bool) -> Result<Vec<String>> {
    let content_filter = if content_only {
        "AND dp.full_path LIKE '%/Content/%'"
    } else {
        ""
    };

    let sql = format!(
        r#"
        {}
        SELECT
            CASE
                WHEN dp.full_path = '/' THEN '/' || sn.text
                WHEN substr(dp.full_path, -1) = '/' THEN dp.full_path || sn.text
                ELSE dp.full_path || '/' || sn.text
            END AS path
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        WHERE LOWER(f.extension) IN ('uasset', 'umap')
        {}
        ORDER BY path
        "#,
        PATH_CTE,
        content_filter
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;

    let mut paths = Vec::new();
    for row in rows {
        paths.push(row?);
    }

    Ok(paths)
}

/// Query a chunk of modules.
/// 查询一批 module。
fn query_files_in_module_chunk(
    conn: &Connection,
    modules: &[String],
    filter: &str,
    limit: usize,
) -> Result<Vec<Value>> {
    if modules.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }

    let placeholders = repeat_placeholders(modules.len());
    let sql = format!(
        r#"
        {}
        SELECT
            CASE
                WHEN dp.full_path = '/' THEN '/' || sn.text
                WHEN substr(dp.full_path, -1) = '/' THEN dp.full_path || sn.text
                ELSE dp.full_path || '/' || sn.text
            END AS file_path,
            f.extension,
            sm.text AS module_name,
            rd.full_path AS module_root
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        JOIN modules m ON f.module_id = m.id
        JOIN strings sm ON m.name_id = sm.id
        JOIN dir_paths rd ON m.root_directory_id = rd.id
        WHERE sm.text IN ({})
          AND file_path LIKE ? ESCAPE '\'
        ORDER BY file_path
        LIMIT ?
        "#,
        PATH_CTE,
        placeholders
    );

    let filter_param = like_contains_pattern(filter);
    let limit_param = limit as i64;

    let mut params: Vec<&dyn ToSql> = modules.iter().map(|module| module as &dyn ToSql).collect();
    params.push(&filter_param);
    params.push(&limit_param);

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), |row| {
        Ok(json!({
            "file_path": row.get::<_, String>(0)?,
            "extension": row.get::<_, String>(1)?,
            "module_name": row.get::<_, String>(2)?,
            "module_root": row.get::<_, String>(3)?,
        }))
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }

    Ok(result)
}

/// Return true if a file contains the target bytes.
/// 如果文件包含目标字节，返回 true。
fn file_contains_bytes(path: &str, needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }

    let Ok(file) = File::open(path) else {
        tracing::debug!("failed to open asset file: {}", path);
        return false;
    };

    let Ok(mmap) = (unsafe { Mmap::map(&file) }) else {
        tracing::debug!("failed to mmap asset file: {}", path);
        return false;
    };

    contains_subslice(&mmap, needle)
}

/// Search for a byte slice inside another byte slice.
/// 在字节切片里搜索另一个字节切片。
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }

    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Convert query rows into a JSON array.
/// 把查询行转换成 JSON 数组。
fn collect_json_rows<T>(rows: rusqlite::MappedRows<T>) -> Result<Value>
where
    T: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<Value>,
{
    let mut values = Vec::new();

    for row in rows {
        values.push(row?);
    }

    Ok(json!(values))
}

/// Build SQL placeholders for an IN clause.
/// 为 IN 子句构建占位符。
fn repeat_placeholders(count: usize) -> String {
    std::iter::repeat("?")
        .take(count)
        .collect::<Vec<_>>()
        .join(",")
}

/// Escape user input for SQL LIKE and wrap it with `%`.
/// 转义 SQL LIKE 用户输入，并用 `%` 包裹。
fn like_contains_pattern(input: &str) -> String {
    format!("%{}%", escape_like(input))
}

/// Escape SQL LIKE special characters.
/// 转义 SQL LIKE 特殊字符。
fn escape_like(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());

    for ch in input.chars() {
        match ch {
            '%' | '_' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }

    escaped
}
