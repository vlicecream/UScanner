use rusqlite::{Connection, ToSql};
use serde_json::{json, Value};
use std::collections::HashSet;

use crate::db::project_path::PATH_CTE;

const DEFAULT_LIMIT: usize = 500;
const STREAM_BATCH_SIZE: usize = 500;

/// Get dependency files included by a source/header file.
/// 获取某个源文件/头文件 include 出来的依赖文件。
pub fn get_depend_files(
    conn: &Connection,
    file_path: &str,
    recursive: bool,
    game_only: bool,
) -> anyhow::Result<Value> {
    let Some(file_id) = find_file_id(conn, file_path)? else {
        return Ok(json!([]));
    };

    let sql = if recursive {
        format!(
            r#"
            {path_cte},
            dependency_graph(file_id, resolved_id) AS (
                SELECT file_id, resolved_file_id
                FROM file_includes
                WHERE file_id = ?

                UNION

                SELECT fi.file_id, fi.resolved_file_id
                FROM file_includes fi
                JOIN dependency_graph dg ON fi.file_id = dg.resolved_id
                WHERE fi.resolved_file_id IS NOT NULL
            )
            SELECT DISTINCT
                dp.full_path || '/' || sn.text AS path,
                sm.text AS module_name,
                rd.full_path AS module_root,
                f.extension
            FROM dependency_graph dg
            JOIN files f ON dg.resolved_id = f.id
            JOIN dir_paths dp ON f.directory_id = dp.id
            JOIN strings sn ON f.filename_id = sn.id
            LEFT JOIN modules m ON f.module_id = m.id
            LEFT JOIN strings sm ON m.name_id = sm.id
            LEFT JOIN dir_paths rd ON m.root_directory_id = rd.id
            WHERE dg.resolved_id IS NOT NULL
            ORDER BY path
            "#,
            path_cte = trim_with_prefix(PATH_CTE)
        )
    } else {
        format!(
            r#"
            {path_cte}
            SELECT DISTINCT
                dp.full_path || '/' || sn.text AS path,
                sm.text AS module_name,
                rd.full_path AS module_root,
                f.extension
            FROM file_includes fi
            JOIN files f ON fi.resolved_file_id = f.id
            JOIN dir_paths dp ON f.directory_id = dp.id
            JOIN strings sn ON f.filename_id = sn.id
            LEFT JOIN modules m ON f.module_id = m.id
            LEFT JOIN strings sm ON m.name_id = sm.id
            LEFT JOIN dir_paths rd ON m.root_directory_id = rd.id
            WHERE fi.file_id = ?
              AND fi.resolved_file_id IS NOT NULL
            ORDER BY path
            "#,
            path_cte = PATH_CTE
        )
    };

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([file_id])?;
    let mut results = Vec::new();

    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;

        if game_only && is_engine_path(&path) {
            continue;
        }

        results.push(json!({
            "file_path": normalize_path(&path),
            "module_name": row.get::<_, Option<String>>(1)?,
            "module_root": row.get::<_, Option<String>>(2)?.map(|p| normalize_path(&p)),
            "extension": row.get::<_, String>(3)?,
        }));
    }

    Ok(json!(results))
}

/// Get files that belong to the given Unreal modules.
/// 获取指定 Unreal 模块下面的文件。
pub fn get_files_in_modules(
    conn: &Connection,
    modules: Vec<String>,
    extensions: Option<Vec<String>>,
    filter: Option<String>,
) -> anyhow::Result<Value> {
    let mut results = Vec::new();

    query_files_in_modules(conn, modules, extensions, filter, |batch| {
        results.extend(batch);
        Ok(())
    })?;

    Ok(json!(results))
}

/// Stream files that belong to the given Unreal modules in batches.
/// 分批返回指定 Unreal 模块下面的文件，避免一次性塞太大的 JSON。
pub fn get_files_in_modules_async<F>(
    conn: &Connection,
    modules: Vec<String>,
    extensions: Option<Vec<String>>,
    filter: Option<String>,
    mut on_items: F,
) -> anyhow::Result<Value>
where
    F: FnMut(Vec<Value>) -> anyhow::Result<()>,
{
    let total = query_files_in_modules(conn, modules, extensions, filter, |batch| {
        on_items(batch)
    })?;

    Ok(json!(total))
}

/// Search files by filename first, then fallback to full path search.
/// 先按文件名搜索，结果太少时再按完整路径兜底搜索。
pub fn search_files_by_path_part(conn: &Connection, part: &str) -> anyhow::Result<Value> {
    let mut results = Vec::new();

    query_files_by_path_part(conn, part, |batch| {
        results.extend(batch);
        Ok(())
    })?;

    Ok(json!(results))
}

/// Stream file search results by path part.
/// 分批返回路径搜索结果。
pub fn search_files_by_path_part_async<F>(
    conn: &Connection,
    part: &str,
    mut on_items: F,
) -> anyhow::Result<Value>
where
    F: FnMut(Vec<Value>) -> anyhow::Result<()>,
{
    let count = query_files_by_path_part(conn, part, |batch| on_items(batch))?;
    Ok(json!(count))
}

/// Get all Unreal Target.cs files.
/// 获取所有 Unreal 的 *.Target.cs 文件。
pub fn get_target_files(conn: &Connection) -> anyhow::Result<Value> {
    let sql = format!(
        r#"
        {}
        SELECT sn.text AS filename,
               dp.full_path || '/' || sn.text AS path
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        WHERE sn.text LIKE '%.Target.cs'
        ORDER BY path
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut results = Vec::new();

    while let Some(row) = rows.next()? {
        results.push(json!({
            "filename": row.get::<_, String>(0)?,
            "path": normalize_path(&row.get::<_, String>(1)?),
        }));
    }

    Ok(json!(results))
}

/// Get all indexed file paths.
/// 获取数据库中所有已索引文件的完整路径。
pub fn get_all_file_paths(conn: &Connection) -> anyhow::Result<Value> {
    let sql = format!(
        r#"
        {}
        SELECT dp.full_path || '/' || sn.text AS path
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        ORDER BY path
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut results = Vec::new();

    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        results.push(Value::String(normalize_path(&path)));
    }

    Ok(json!(results))
}

/// Get all indexed files with basic metadata.
/// 获取所有文件的基础元数据：文件名、路径、模块名。
pub fn get_all_files_metadata(conn: &Connection) -> anyhow::Result<Value> {
    let sql = format!(
        r#"
        {}
        SELECT sn.text AS filename,
               dp.full_path || '/' || sn.text AS path,
               sm.text AS module_name
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        LEFT JOIN modules m ON f.module_id = m.id
        LEFT JOIN strings sm ON m.name_id = sm.id
        ORDER BY path
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut results = Vec::new();

    while let Some(row) = rows.next()? {
        results.push(json!({
            "filename": row.get::<_, String>(0)?,
            "path": normalize_path(&row.get::<_, String>(1)?),
            "module_name": row.get::<_, Option<String>>(2)?,
        }));
    }

    Ok(json!(results))
}

/// Shared implementation for module file queries.
/// 模块文件查询的共享实现，同步和流式接口都走这里。
fn query_files_in_modules<F>(
    conn: &Connection,
    modules: Vec<String>,
    extensions: Option<Vec<String>>,
    filter: Option<String>,
    mut on_batch: F,
) -> anyhow::Result<usize>
where
    F: FnMut(Vec<Value>) -> anyhow::Result<()>,
{
    if modules.is_empty() {
        return Ok(0);
    }

    let extension_set = extensions.map(|items| {
        items
            .into_iter()
            .map(|item| item.trim_start_matches('.').to_ascii_lowercase())
            .collect::<HashSet<_>>()
    });

    let filter = filter
        .map(|s| s.replace('\\', "/"))
        .filter(|s| !s.is_empty());

    let mut total = 0usize;
    let mut batch = Vec::new();

    for module_chunk in modules.chunks(500) {
        let placeholders = repeat_placeholders(module_chunk.len());
        let sql = format!(
            r#"
            {}
            SELECT dp.full_path || '/' || sn.text AS path,
                   sm.text AS module_name,
                   rd.full_path AS module_root,
                   f.extension
            FROM files f
            JOIN dir_paths dp ON f.directory_id = dp.id
            JOIN strings sn ON f.filename_id = sn.id
            JOIN modules m ON f.module_id = m.id
            JOIN strings sm ON m.name_id = sm.id
            JOIN dir_paths rd ON m.root_directory_id = rd.id
            WHERE sm.text IN ({})
            ORDER BY path
            "#,
            PATH_CTE,
            placeholders
        );

        let params: Vec<&dyn ToSql> = module_chunk.iter().map(|m| m as &dyn ToSql).collect();

        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;

        while let Some(row) = rows.next()? {
            let path = normalize_path(&row.get::<_, String>(0)?);
            let ext = row.get::<_, String>(3)?.to_ascii_lowercase();

            if let Some(ref allowed) = extension_set {
                if !allowed.contains(&ext) {
                    continue;
                }
            }

            if let Some(ref text) = filter {
                if !path.contains(text) {
                    continue;
                }
            }

            batch.push(json!({
                "file_path": path,
                "module_name": row.get::<_, String>(1)?,
                "module_root": normalize_path(&row.get::<_, String>(2)?),
                "extension": ext,
            }));

            total += 1;

            if batch.len() >= STREAM_BATCH_SIZE {
                on_batch(std::mem::take(&mut batch))?;
            }
        }
    }

    if !batch.is_empty() {
        on_batch(batch)?;
    }

    Ok(total)
}

/// Shared implementation for file path search.
/// 文件路径搜索的共享实现。
fn query_files_by_path_part<F>(
    conn: &Connection,
    part: &str,
    mut on_batch: F,
) -> anyhow::Result<usize>
where
    F: FnMut(Vec<Value>) -> anyhow::Result<()>,
{
    let part = part.trim();
    if part.is_empty() {
        return Ok(0);
    }

    let pattern = format!("%{}%", escape_like(part));
    let mut seen_paths = HashSet::new();
    let mut batch = Vec::new();
    let mut total = 0usize;

    let filename_sql = format!(
        r#"
        {}
        SELECT sn.text AS filename,
               dp.full_path || '/' || sn.text AS path,
               sm.text AS module_name,
               rd.full_path AS module_root
        FROM files f
        JOIN strings sn ON f.filename_id = sn.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        LEFT JOIN modules m ON f.module_id = m.id
        LEFT JOIN strings sm ON m.name_id = sm.id
        LEFT JOIN dir_paths rd ON m.root_directory_id = rd.id
        WHERE sn.text LIKE ? ESCAPE '\'
        ORDER BY sn.text
        LIMIT ?
        "#,
        PATH_CTE
    );

    collect_file_search_rows(
        conn,
        &filename_sql,
        &pattern,
        DEFAULT_LIMIT as i64,
        &mut seen_paths,
        &mut batch,
        &mut total,
        &mut on_batch,
    )?;

    if total < 50 {
        let full_path_sql = format!(
            r#"
            {}
            SELECT sn.text AS filename,
                   dp.full_path || '/' || sn.text AS path,
                   sm.text AS module_name,
                   rd.full_path AS module_root
            FROM files f
            JOIN strings sn ON f.filename_id = sn.id
            JOIN dir_paths dp ON f.directory_id = dp.id
            LEFT JOIN modules m ON f.module_id = m.id
            LEFT JOIN strings sm ON m.name_id = sm.id
            LEFT JOIN dir_paths rd ON m.root_directory_id = rd.id
            WHERE (dp.full_path || '/' || sn.text) LIKE ? ESCAPE '\'
            ORDER BY path
            LIMIT ?
            "#,
            PATH_CTE
        );

        collect_file_search_rows(
            conn,
            &full_path_sql,
            &pattern,
            DEFAULT_LIMIT as i64,
            &mut seen_paths,
            &mut batch,
            &mut total,
            &mut on_batch,
        )?;
    }

    if !batch.is_empty() {
        on_batch(batch)?;
    }

    Ok(total)
}

/// Collect rows returned by a file search SQL statement.
/// 收集文件搜索 SQL 返回的结果，并做去重和分批。
fn collect_file_search_rows<F>(
    conn: &Connection,
    sql: &str,
    pattern: &str,
    limit: i64,
    seen_paths: &mut HashSet<String>,
    batch: &mut Vec<Value>,
    total: &mut usize,
    on_batch: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<Value>) -> anyhow::Result<()>,
{
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(rusqlite::params![pattern, limit])?;

    while let Some(row) = rows.next()? {
        let path = normalize_path(&row.get::<_, String>(1)?);

        if !seen_paths.insert(path.clone()) {
            continue;
        }

        batch.push(json!({
            "filename": row.get::<_, String>(0)?,
            "path": path,
            "module_name": row.get::<_, Option<String>>(2)?,
            "module_root": row.get::<_, Option<String>>(3)?.map(|p| normalize_path(&p)),
        }));

        *total += 1;

        if batch.len() >= STREAM_BATCH_SIZE {
            on_batch(std::mem::take(batch))?;
        }

        if *total >= DEFAULT_LIMIT {
            break;
        }
    }

    Ok(())
}

/// Find a file id by full path first, then fallback to filename.
/// 先按完整路径找文件，找不到再按文件名兜底。
fn find_file_id(conn: &Connection, file_path: &str) -> anyhow::Result<Option<i64>> {
    let normalized = normalize_path(file_path);

    let full_path_sql = format!(
        r#"
        {}
        SELECT f.id
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        WHERE dp.full_path || '/' || sn.text = ?
        LIMIT 1
        "#,
        PATH_CTE
    );

    if let Ok(id) = conn.query_row(&full_path_sql, [&normalized], |row| row.get::<_, i64>(0)) {
        return Ok(Some(id));
    }

    let filename = std::path::Path::new(file_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if filename.is_empty() {
        return Ok(None);
    }

    let id = conn
        .query_row(
            r#"
            SELECT f.id
            FROM files f
            JOIN strings sn ON f.filename_id = sn.id
            WHERE sn.text = ?
            LIMIT 1
            "#,
            [filename],
            |row| row.get::<_, i64>(0),
        )
        .ok();

    Ok(id)
}

/// Create SQL placeholders like "?,?,?".
/// 生成 SQL 参数占位符，比如 "?,?,?"。
fn repeat_placeholders(count: usize) -> String {
    std::iter::repeat("?")
        .take(count)
        .collect::<Vec<_>>()
        .join(",")
}

/// Escape LIKE wildcards so user input is treated as plain text.
/// 转义 LIKE 通配符，避免用户输入的 % 或 _ 被当成匹配规则。
fn escape_like(input: &str) -> String {
    input
        .replace('\\', r"\\")
        .replace('%', r"\%")
        .replace('_', r"\_")
}

/// Normalize Windows paths to slash-separated paths.
/// 把 Windows 反斜杠路径统一成斜杠路径。
fn normalize_path(path: &str) -> String {
    path.replace('\\', "/").replace("//", "/")
}

/// Return true if the path belongs to Unreal Engine instead of game code.
/// 判断路径是否属于 Engine，而不是游戏工程代码。
fn is_engine_path(path: &str) -> bool {
    let normalized = normalize_path(path).to_ascii_lowercase();
    normalized.contains("/engine/")
}

/// Convert a WITH CTE into a form that can append another CTE.
/// 把已有 WITH CTE 转成可以继续追加 CTE 的形式。
fn trim_with_prefix(path_cte: &str) -> String {
    path_cte.trim().trim_end_matches(',').to_string()
}
