use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::db::project_path::PATH_CTE;

/// Search indexed symbols by name using SQLite FTS.
/// 使用 SQLite FTS 根据符号名搜索已索引的 symbol。
pub fn search_symbols(conn: &Connection, pattern: &str, limit: usize) -> anyhow::Result<Value> {
    let pattern = pattern.trim();

    if pattern.is_empty() {
        return list_symbols(conn, limit);
    }

    let limit = limit.clamp(1, 10_000) as i64;
    let query = build_fts_query(pattern);

    let sql = format!(
        r#"
        {}
        SELECT
            sfts.name,
            sfts.type,
            sfts.class_name,
            dp.full_path || '/' || sn.text AS path,
            c.line_number,
            sm.text AS module_name
        FROM symbols_fts sfts
        JOIN classes c ON sfts.rowid_ref = c.id
        JOIN files f ON c.file_id = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        LEFT JOIN modules m ON f.module_id = m.id
        LEFT JOIN strings sm ON m.name_id = sm.id
        WHERE symbols_fts MATCH ?
        ORDER BY rank
        LIMIT ?
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![query, limit], |row| {
        Ok(json!({
            "name": row.get::<_, String>(0)?,
            "type": row.get::<_, String>(1)?,
            "class_name": row.get::<_, String>(2)?,
            "path": normalize_path(&row.get::<_, String>(3)?),
            "line": row.get::<_, Option<i64>>(4)?,
            "module_name": row.get::<_, Option<String>>(5)?,
        }))
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }

    Ok(json!(results))
}

/// Return indexed symbols for interactive picker-side fuzzy filtering.
/// 返回索引符号，供前端 picker 做本地模糊过滤。
fn list_symbols(conn: &Connection, limit: usize) -> anyhow::Result<Value> {
    let limit = limit.clamp(1, 10_000) as i64;

    let sql = format!(
        r#"
        {}
        SELECT
            sc.text AS name,
            c.symbol_type,
            NULL AS class_name,
            dp.full_path || '/' || sn.text AS path,
            c.line_number,
            sm.text AS module_name
        FROM classes c
        JOIN strings sc ON c.name_id = sc.id
        JOIN files f ON c.file_id = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        LEFT JOIN modules m ON f.module_id = m.id
        LEFT JOIN strings sm ON m.name_id = sm.id
        WHERE sc.text NOT LIKE '(%'
        ORDER BY sc.text ASC
        LIMIT ?
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([limit], |row| {
        Ok(json!({
            "name": row.get::<_, String>(0)?,
            "type": row.get::<_, String>(1)?,
            "class_name": row.get::<_, Option<String>>(2)?,
            "path": normalize_path(&row.get::<_, String>(3)?),
            "line": row.get::<_, Option<i64>>(4)?,
            "module_name": row.get::<_, Option<String>>(5)?,
        }))
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }

    Ok(json!(results))
}

/// Get all indexed C++/Unreal structs.
/// 获取所有已经索引到的 C++/Unreal struct。
pub fn get_structs(conn: &Connection) -> anyhow::Result<Value> {
    let sql = format!(
        r#"
        {}
        SELECT
            sc.text AS name,
            sb.text AS base_class,
            c.symbol_type,
            dp.full_path || '/' || sn.text AS path,
            sm.text AS module_name,
            c.line_number,
            c.end_line_number
        FROM classes c
        JOIN strings sc ON c.name_id = sc.id
        LEFT JOIN strings sb ON c.base_class_id = sb.id
        JOIN files f ON c.file_id = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        LEFT JOIN modules m ON f.module_id = m.id
        LEFT JOIN strings sm ON m.name_id = sm.id
        WHERE c.symbol_type IN ('struct', 'USTRUCT')
          AND sc.text NOT LIKE '(%'
        ORDER BY sc.text ASC
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "name": row.get::<_, String>(0)?,
            "base_class": row.get::<_, Option<String>>(1)?,
            "type": row.get::<_, String>(2)?,
            "path": normalize_path(&row.get::<_, String>(3)?),
            "module_name": row.get::<_, Option<String>>(4)?,
            "line": row.get::<_, Option<i64>>(5)?,
            "end_line": row.get::<_, Option<i64>>(6)?,
        }))
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }

    Ok(json!(results))
}

/// Get all indexed classes, structs, or enums by symbol type.
/// 按 symbol_type 获取 class、struct、enum 等类型符号。
pub fn get_symbols_by_type(
    conn: &Connection,
    symbol_type: &str,
    limit: Option<usize>,
) -> anyhow::Result<Value> {
    let symbol_type = symbol_type.trim();

    if symbol_type.is_empty() {
        return Ok(json!([]));
    }

    let limit = limit.unwrap_or(1000).clamp(1, 5000) as i64;

    let sql = format!(
        r#"
        {}
        SELECT
            sc.text AS name,
            sb.text AS base_class,
            c.symbol_type,
            dp.full_path || '/' || sn.text AS path,
            sm.text AS module_name,
            c.line_number,
            c.end_line_number
        FROM classes c
        JOIN strings sc ON c.name_id = sc.id
        LEFT JOIN strings sb ON c.base_class_id = sb.id
        JOIN files f ON c.file_id = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        LEFT JOIN modules m ON f.module_id = m.id
        LEFT JOIN strings sm ON m.name_id = sm.id
        WHERE c.symbol_type = ?
          AND sc.text NOT LIKE '(%'
        ORDER BY sc.text ASC
        LIMIT ?
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![symbol_type, limit], |row| {
        Ok(json!({
            "name": row.get::<_, String>(0)?,
            "base_class": row.get::<_, Option<String>>(1)?,
            "type": row.get::<_, String>(2)?,
            "path": normalize_path(&row.get::<_, String>(3)?),
            "module_name": row.get::<_, Option<String>>(4)?,
            "line": row.get::<_, Option<i64>>(5)?,
            "end_line": row.get::<_, Option<i64>>(6)?,
        }))
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }

    Ok(json!(results))
}

/// Build a safe SQLite FTS query from user input.
/// 根据用户输入构造相对安全的 SQLite FTS 查询。
fn build_fts_query(input: &str) -> String {
    let tokens = input
        .split_whitespace()
        .map(clean_fts_token)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();

    if tokens.is_empty() {
        return clean_fts_token(input);
    }

    tokens
        .into_iter()
        .map(|token| format!("{}*", token))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Remove FTS syntax characters from one search token.
/// 清理单个搜索 token 里的 FTS 语法字符。
fn clean_fts_token(input: &str) -> String {
    input
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
        .collect()
}

/// Normalize Windows paths to slash-separated paths.
/// 把 Windows 反斜杠路径统一成斜杠路径。
fn normalize_path(path: &str) -> String {
    path.replace('\\', "/").replace("//", "/")
}
