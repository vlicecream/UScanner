use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};
use serde_json::{json, Value};

use crate::db::project_path::PATH_CTE;

const MODULE_CHUNK_SIZE: usize = 500;
const STREAM_BATCH_SIZE: usize = 200;
const DEFAULT_USAGE_LIMIT: usize = 200;

/// Return all symbols declared in one file.
/// 返回某个文件里声明的所有符号。
pub fn get_file_symbols(conn: &Connection, file_path: &str) -> Result<Value> {
    let Some(file_id) = find_file_id(conn, file_path)? else {
        return Ok(json!([]));
    };

    let mut class_stmt = conn.prepare(
        r#"
        SELECT
            c.id,
            sc.text AS name,
            c.line_number,
            c.symbol_type,
            c.end_line_number
        FROM classes c
        JOIN strings sc ON c.name_id = sc.id
        WHERE c.file_id = ?1
        ORDER BY c.line_number
        "#,
    )?;

    let member_sql = format!(
        r#"
        {}
        SELECT
            sn.text AS name,
            st.text AS type,
            m.access,
            m.flags,
            m.line_number,
            m.detail,
            srt.text AS return_type,
            m.is_static,
            COALESCE({}, '') AS file_path
        FROM members m
        JOIN strings sn ON m.name_id = sn.id
        JOIN strings st ON m.type_id = st.id
        LEFT JOIN strings srt ON m.return_type_id = srt.id
        LEFT JOIN files mf ON m.file_id = mf.id
        LEFT JOIN dir_paths dp ON mf.directory_id = dp.id
        LEFT JOIN strings sf ON mf.filename_id = sf.id
        WHERE m.class_id = ?1
        ORDER BY m.line_number
        "#,
        PATH_CTE,
        file_path_expr("dp", "sf"),
    );

    let mut member_stmt = conn.prepare(&member_sql)?;
    let mut class_rows = class_stmt.query([file_id])?;
    let mut results = Vec::new();

    while let Some(row) = class_rows.next()? {
        let class_id: i64 = row.get(0)?;
        let mut member_rows = member_stmt.query([class_id])?;
        let mut members = Vec::new();

        while let Some(member_row) = member_rows.next()? {
            let member_file_path: String = member_row.get(8)?;

            members.push(json!({
                "name": member_row.get::<_, String>(0)?,
                "type": member_row.get::<_, String>(1)?,
                "access": member_row.get::<_, String>(2)?,
                "flags": member_row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                "line": member_row.get::<_, i64>(4)?,
                "detail": member_row.get::<_, Option<String>>(5)?,
                "return_type": member_row.get::<_, Option<String>>(6)?,
                "is_static": member_row.get::<_, i64>(7)? == 1,
                "file_path": if member_file_path.is_empty() {
                    normalize_path(file_path)
                } else {
                    normalize_path(&member_file_path)
                },
            }));
        }

        results.push(json!({
            "name": row.get::<_, String>(1)?,
            "line": row.get::<_, i64>(2)?,
            "kind": row.get::<_, String>(3)?,
            "end_line": row.get::<_, i64>(4)?,
            "file_path": normalize_path(file_path),
            "members": members,
        }));
    }

    Ok(json!(results))
}

/// Return members of every class with the given name.
/// 返回指定类名的所有成员。
pub fn get_class_members(conn: &Connection, class_name: &str) -> Result<Value> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            m.name_id,
            sn.text AS name,
            st.text AS type,
            m.access,
            m.flags,
            m.line_number,
            m.detail,
            srt.text AS return_type,
            m.is_static
        FROM members m
        JOIN strings sn ON m.name_id = sn.id
        JOIN strings st ON m.type_id = st.id
        LEFT JOIN strings srt ON m.return_type_id = srt.id
        JOIN classes c ON m.class_id = c.id
        JOIN strings sc ON c.name_id = sc.id
        WHERE sc.text = ?1
        ORDER BY m.line_number
        "#,
    )?;

    let rows = stmt.query_map([class_name], |row| {
        Ok(json!({
            "name": row.get::<_, String>(1)?,
            "type": row.get::<_, String>(2)?,
            "access": row.get::<_, String>(3)?,
            "flags": row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            "line": row.get::<_, i64>(5)?,
            "detail": row.get::<_, Option<String>>(6)?,
            "return_type": row.get::<_, Option<String>>(7)?,
            "is_static": row.get::<_, i64>(8)? == 1,
        }))
    })?;

    collect_json_rows(rows)
}

/// Search class-like symbols by name prefix.
/// 按名称前缀搜索 class/struct/enum 等类型符号。
pub fn search_classes_prefix(
    conn: &Connection,
    prefix: &str,
    limit: Option<usize>,
) -> Result<Value> {
    let prefix = prefix.trim();

    if prefix.is_empty() {
        return Ok(json!([]));
    }

    let limit = limit.unwrap_or(50).clamp(1, 1000) as i64;
    let pattern = format!("{}%", prefix);

    let sql = format!(
        r#"
        {}
        SELECT
            c.id,
            sc.text AS name,
            sb.text AS base_class,
            c.symbol_type,
            {} AS path,
            sm.text AS module_name,
            c.line_number,
            c.end_line_number
        FROM classes c
        JOIN strings sc ON c.name_id = sc.id
        LEFT JOIN strings sb ON c.base_class_id = sb.id
        LEFT JOIN files f ON c.file_id = f.id
        LEFT JOIN dir_paths dp ON f.directory_id = dp.id
        LEFT JOIN strings sf ON f.filename_id = sf.id
        LEFT JOIN modules m ON f.module_id = m.id
        LEFT JOIN strings sm ON m.name_id = sm.id
        WHERE sc.text LIKE ?1
          AND sc.text NOT LIKE '(%'
        ORDER BY sc.text ASC
        LIMIT ?2
        "#,
        PATH_CTE,
        file_path_expr("dp", "sf"),
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![pattern, limit], |row| {
        let path = row
            .get::<_, Option<String>>(4)?
            .map(|value| normalize_path(&value));

        Ok(json!({
            "id": row.get::<_, i64>(0)?,
            "name": row.get::<_, String>(1)?,
            "base_class": row.get::<_, Option<String>>(2)?,
            "type": row.get::<_, String>(3)?,
            "path": path,
            "module_name": row.get::<_, Option<String>>(5)?,
            "line": row.get::<_, Option<i64>>(6)?,
            "end_line": row.get::<_, Option<i64>>(7)?,
        }))
    })?;

    collect_json_rows(rows)
}

/// Return classes grouped by file for selected modules.
/// 返回指定模块中的类，并按文件分组。
///
/// Return shape:
/// 返回格式：
/// `{ "p": path, "i": [[name, line, type, base], ...] }`
pub fn get_classes_in_modules(
    conn: &Connection,
    modules: Vec<String>,
    symbol_type: Option<String>,
) -> Result<Value> {
    if modules.is_empty() {
        return Ok(json!([]));
    }

    let mut grouped: BTreeMap<String, Vec<Value>> = BTreeMap::new();

    for chunk in modules.chunks(MODULE_CHUNK_SIZE) {
        let rows = query_classes_in_module_chunk(conn, chunk, symbol_type.as_deref())?;

        for item in rows {
            let path = item.path;
            grouped
                .entry(path)
                .or_default()
                .push(json!([item.name, item.line, item.symbol_type, item.base]));
        }
    }

    let result = grouped
        .into_iter()
        .map(|(path, items)| json!({ "p": path, "i": items }))
        .collect::<Vec<_>>();

    Ok(json!(result))
}

/// Stream classes from selected modules in batches.
/// 分批返回指定模块中的类。
pub fn get_classes_in_modules_async<F>(
    conn: &Connection,
    modules: Vec<String>,
    symbol_type: Option<String>,
    mut on_items: F,
) -> Result<Value>
where
    F: FnMut(Vec<Value>) -> Result<()>,
{
    if modules.is_empty() {
        return Ok(json!(0));
    }

    let mut total_sent = 0usize;

    for chunk in modules.chunks(MODULE_CHUNK_SIZE) {
        let rows = query_classes_in_module_chunk(conn, chunk, symbol_type.as_deref())?;
        let mut batch = Vec::with_capacity(STREAM_BATCH_SIZE);

        for item in rows {
            batch.push(json!({
                "name": item.name,
                "base": item.base,
                "path": item.path,
                "line": item.line,
                "type": item.symbol_type,
            }));

            if batch.len() >= STREAM_BATCH_SIZE {
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

/// Return values of an enum-like class entry.
/// 返回 enum-like class entry 的枚举值。
///
/// It tries exact name first, then Unreal-style fallback forms.
/// 会先按完整名称查找，然后尝试 Unreal 风格 fallback。
pub fn get_enum_values(conn: &Connection, enum_name: &str) -> Result<Value> {
    let candidates = enum_name_candidates(enum_name);

    let mut class_id = None;
    for candidate in candidates {
        class_id = find_class_id_by_name(conn, &candidate)?;
        if class_id.is_some() {
            break;
        }
    }

    let Some(class_id) = class_id else {
        return Ok(json!([]));
    };

    let mut stmt = conn.prepare(
        r#"
        SELECT s.text
        FROM enum_values ev
        JOIN strings s ON ev.name_id = s.id
        WHERE ev.enum_id = ?1
        ORDER BY ev.line_number ASC
        "#,
    )?;

    let rows = stmt.query_map([class_id], |row| {
        Ok(json!(row.get::<_, String>(0)?))
    })?;

    collect_json_rows(rows)
}

/// Find usages recorded in `symbol_calls`.
/// 从 `symbol_calls` 表查找符号调用位置。
pub fn find_symbol_usages(
    conn: &Connection,
    symbol_name: &str,
    limit: usize,
) -> Result<Value> {
    let limit = if limit == 0 {
        DEFAULT_USAGE_LIMIT
    } else {
        limit
    };

    let sql = format!(
        r#"
        {}
        SELECT
            sc.line,
            {} AS path
        FROM symbol_calls sc
        JOIN strings s ON sc.name_id = s.id
        JOIN files f ON sc.file_id = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        WHERE s.text = ?1
        ORDER BY path, sc.line
        LIMIT ?2
        "#,
        PATH_CTE,
        file_path_expr("dp", "sn"),
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![symbol_name, limit as i64], |row| {
        Ok(json!({
            "line": row.get::<_, i64>(0)?,
            "path": normalize_path(&row.get::<_, String>(1)?),
        }))
    })?;

    collect_json_rows(rows)
}

/// Class row returned from module queries.
/// 模块 class 查询返回的中间结构。
struct ClassModuleItem {
    name: String,
    base: Option<String>,
    path: String,
    line: i64,
    symbol_type: String,
}

/// Query one module chunk.
/// 查询一批 module。
fn query_classes_in_module_chunk(
    conn: &Connection,
    modules: &[String],
    symbol_type: Option<&str>,
) -> Result<Vec<ClassModuleItem>> {
    if modules.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = repeat_placeholders(modules.len());
    let type_clause = if symbol_type.is_some() {
        " AND c.symbol_type = ?"
    } else {
        ""
    };

    let sql = format!(
        r#"
        {}
        SELECT
            sc.text AS name,
            sb.text AS base,
            {} AS path,
            c.line_number,
            c.symbol_type
        FROM classes c
        JOIN strings sc ON c.name_id = sc.id
        LEFT JOIN strings sb ON c.base_class_id = sb.id
        JOIN files f ON c.file_id = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sf ON f.filename_id = sf.id
        JOIN modules m ON f.module_id = m.id
        JOIN strings sm ON m.name_id = sm.id
        WHERE sm.text IN ({}){}
        ORDER BY path, c.line_number
        "#,
        PATH_CTE,
        file_path_expr("dp", "sf"),
        placeholders,
        type_clause,
    );

    let mut query_params = modules.to_vec();

    if let Some(symbol_type) = symbol_type {
        query_params.push(symbol_type.to_string());
    }

    let query_refs: Vec<&dyn ToSql> = query_params
        .iter()
        .map(|value| value as &dyn ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(query_refs))?;
    let mut result = Vec::new();

    while let Some(row) = rows.next()? {
        result.push(ClassModuleItem {
            name: row.get(0)?,
            base: row.get(1)?,
            path: normalize_path(&row.get::<_, String>(2)?),
            line: row.get(3)?,
            symbol_type: row.get(4)?,
        });
    }

    Ok(result)
}

/// Find file id by full path, then fallback to filename.
/// 先通过完整路径查找 file id，失败后退回文件名查找。
fn find_file_id(conn: &Connection, file_path: &str) -> Result<Option<i64>> {
    let normalized = normalize_path(file_path);

    let sql = format!(
        r#"
        {}
        SELECT f.id
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sf ON f.filename_id = sf.id
        WHERE {} = ?1
        LIMIT 1
        "#,
        PATH_CTE,
        file_path_expr("dp", "sf"),
    );

    let exact = conn
        .query_row(&sql, [normalized.as_str()], |row| row.get(0))
        .optional()?;

    if exact.is_some() {
        return Ok(exact);
    }

    let filename = std::path::Path::new(file_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");

    if filename.is_empty() {
        return Ok(None);
    }

    conn.query_row(
        r#"
        SELECT f.id
        FROM files f
        JOIN strings s ON f.filename_id = s.id
        WHERE s.text = ?1
        ORDER BY f.id
        LIMIT 1
        "#,
        [filename],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Find class id by class name.
/// 根据类名查找 class id。
fn find_class_id_by_name(conn: &Connection, class_name: &str) -> Result<Option<i64>> {
    conn.query_row(
        r#"
        SELECT c.id
        FROM classes c
        JOIN strings sn ON c.name_id = sn.id
        WHERE sn.text = ?1
        LIMIT 1
        "#,
        [class_name],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Generate enum lookup candidates.
/// 生成 enum 查询候选名。
fn enum_name_candidates(enum_name: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    candidates.push(enum_name.to_string());

    if let Some(pos) = enum_name.rfind("::") {
        let left = &enum_name[..pos];
        let right = &enum_name[pos + 2..];

        if !left.is_empty() {
            candidates.push(left.to_string());
        }

        if !right.is_empty() {
            candidates.push(right.to_string());
        }
    }

    candidates.dedup();
    candidates
}

/// Build a robust file path SQL expression.
/// 构建更稳的文件路径 SQL 表达式。
fn file_path_expr(dir_alias: &str, name_alias: &str) -> String {
    format!(
        r#"
        CASE
            WHEN {dir}.full_path = '/' THEN '/' || {name}.text
            WHEN substr({dir}.full_path, -1) = '/' THEN {dir}.full_path || {name}.text
            ELSE {dir}.full_path || '/' || {name}.text
        END
        "#,
        dir = dir_alias,
        name = name_alias,
    )
}

/// Build SQL placeholders for `IN (...)`.
/// 为 `IN (...)` 构建 SQL 占位符。
fn repeat_placeholders(count: usize) -> String {
    std::iter::repeat("?")
        .take(count)
        .collect::<Vec<_>>()
        .join(",")
}

/// Convert mapped rows into a JSON array.
/// 把 mapped rows 转成 JSON 数组。
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

/// Normalize path separators for JSON output and comparison.
/// 规范化路径分隔符，便于 JSON 输出和比较。
fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}
