use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection};
use serde_json::{json, Value};

use crate::db::project_path::PATH_CTE;

/// Search indexed symbols with database-side ranking and pagination.
/// 使用数据库侧排序和分页搜索已索引的 symbol。
///
/// Ranking intentionally favors human "global find" expectations:
/// - exact name matches first;
/// - then name prefix matches;
/// - then continuous substring matches in the symbol name;
/// - then owner class / module / path matches;
/// - only after those should picker-side fuzzy matching matter.
///
/// This prevents a query such as `death` from being dominated by loose
/// `d ... e ... a ... t ... h` fuzzy results before real `Death*` symbols.
/// 这里的优先级是给全局搜索用的：完全匹配、前缀匹配、连续子串匹配优先，
/// 最后才让前端 fuzzy 参与，避免 `death` 被松散的字符级匹配结果压到后面。
pub fn search_symbols(
    conn: &Connection,
    pattern: &str,
    limit: usize,
    offset: usize,
) -> anyhow::Result<Value> {
    let pattern = pattern.trim();

    if pattern.is_empty() {
        return list_symbols(conn, limit, offset);
    }

    let limit = limit.clamp(1, 10_000) as i64;
    let offset = offset.min(1_000_000) as i64;
    let query = pattern.to_ascii_lowercase();
    let prefix_query = format!("{}%", escape_like(&query));
    let contains_query = format!("%{}%", escape_like(&query));
    let tokens = search_tokens(pattern);
    let searchable = searchable_sql();
    let token_filter = tokens
        .iter()
        .map(|_| format!("{searchable} LIKE ? ESCAPE '\\'"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let where_clause = if token_filter.is_empty() {
        "1 = 1".to_string()
    } else {
        token_filter
    };

    let sql = format!(
        r#"
        {}
        SELECT
            sfts.name,
            sfts.type,
            sfts.class_name,
            dp.full_path || '/' || sn.text AS path,
            COALESCE(c.line_number, mem.line_number),
            sm.text AS module_name
        FROM symbols_fts sfts
        LEFT JOIN classes c
            ON c.id = sfts.rowid_ref
           AND {}
        LEFT JOIN members mem
            ON mem.id = sfts.rowid_ref
           AND NOT ({})
        JOIN files f ON f.id = COALESCE(c.file_id, mem.file_id)
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        LEFT JOIN modules m ON f.module_id = m.id
        LEFT JOIN strings sm ON m.name_id = sm.id
        WHERE {}
        ORDER BY
            CASE
                WHEN lower(sfts.name) = ? THEN 0
                WHEN lower(sfts.name) LIKE ? ESCAPE '\' THEN 1
                WHEN lower(sfts.name) LIKE ? ESCAPE '\' THEN 2
                WHEN lower(COALESCE(sfts.class_name, '')) LIKE ? ESCAPE '\' THEN 3
                WHEN lower(COALESCE(sm.text, '')) LIKE ? ESCAPE '\' THEN 4
                WHEN lower(dp.full_path || '/' || sn.text) LIKE ? ESCAPE '\' THEN 5
                ELSE 9
            END,
            CASE
                WHEN COALESCE(sfts.type, '') IN ('class', 'struct', 'enum', 'UCLASS', 'USTRUCT', 'UENUM') THEN 0
                WHEN lower(COALESCE(sfts.type, '')) LIKE '%function%' OR lower(COALESCE(sfts.type, '')) LIKE '%method%' THEN 1
                ELSE 2
            END,
            lower(sfts.name) ASC,
            path ASC
        LIMIT ? OFFSET ?
        "#,
        PATH_CTE,
        class_symbol_predicate("sfts"),
        class_symbol_predicate("sfts"),
        where_clause
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut params = Vec::new();
    for token in &tokens {
        params.push(SqlValue::Text(format!("%{}%", escape_like(token))));
    }
    params.push(SqlValue::Text(query));
    params.push(SqlValue::Text(prefix_query));
    params.push(SqlValue::Text(contains_query.clone()));
    params.push(SqlValue::Text(contains_query.clone()));
    params.push(SqlValue::Text(contains_query.clone()));
    params.push(SqlValue::Text(contains_query));
    params.push(SqlValue::Integer(limit));
    params.push(SqlValue::Integer(offset));

    let rows = stmt.query_map(params_from_iter(params), |row| {
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

/// Return indexed symbols for interactive picker-side fuzzy filtering.
/// 返回索引符号，供前端 picker 做本地模糊过滤。
fn list_symbols(conn: &Connection, limit: usize, offset: usize) -> anyhow::Result<Value> {
    let limit = limit.clamp(1, 10_000) as i64;
    let offset = offset.min(1_000_000) as i64;

    let sql = format!(
        r#"
        {}
        SELECT
            sfts.name,
            sfts.type,
            sfts.class_name,
            dp.full_path || '/' || sn.text AS path,
            COALESCE(c.line_number, mem.line_number),
            sm.text AS module_name
        FROM symbols_fts sfts
        LEFT JOIN classes c
            ON c.id = sfts.rowid_ref
           AND {}
        LEFT JOIN members mem
            ON mem.id = sfts.rowid_ref
           AND NOT ({})
        JOIN files f ON f.id = COALESCE(c.file_id, mem.file_id)
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        LEFT JOIN modules m ON f.module_id = m.id
        LEFT JOIN strings sm ON m.name_id = sm.id
        WHERE sfts.name NOT LIKE '(%'
        ORDER BY lower(sfts.name) ASC
        LIMIT ? OFFSET ?
        "#,
        PATH_CTE,
        class_symbol_predicate("sfts"),
        class_symbol_predicate("sfts")
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit, offset], |row| {
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

fn class_symbol_predicate(alias: &str) -> String {
    format!(
        "COALESCE({alias}.type, '') IN ('class', 'struct', 'enum', 'UCLASS', 'USTRUCT', 'UENUM')"
    )
}

fn searchable_sql() -> &'static str {
    "lower(
        COALESCE(sfts.name, '') || ' ' ||
        COALESCE(sfts.type, '') || ' ' ||
        COALESCE(sfts.class_name, '') || ' ' ||
        COALESCE(sm.text, '') || ' ' ||
        COALESCE(dp.full_path, '') || '/' || COALESCE(sn.text, '')
    )"
}

fn search_tokens(input: &str) -> Vec<String> {
    input
        .split_whitespace()
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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

/// Normalize Windows paths to slash-separated paths.
/// 把 Windows 反斜杠路径统一成斜杠路径。
fn normalize_path(path: &str) -> String {
    path.replace('\\', "/").replace("//", "/")
}
