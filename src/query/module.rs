use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::db::project_path::PATH_CTE;

/// Get all indexed Unreal modules.
/// 获取所有已经索引到的 Unreal 模块。
pub fn get_modules(conn: &Connection) -> anyhow::Result<Value> {
    let sql = format!(
        r#"
        {}
        SELECT
            m.id,
            sm.text AS name,
            m.type,
            m.scope,
            dp.full_path AS root_path,
            m.build_cs_path,
            m.owner_name,
            m.component_name,
            m.deep_dependencies
        FROM modules m
        JOIN strings sm ON m.name_id = sm.id
        JOIN dir_paths dp ON m.root_directory_id = dp.id
        ORDER BY sm.text
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], module_row_to_json)?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }

    Ok(json!(results))
}

/// Get one Unreal module by exact module name.
/// 根据模块名精确获取一个 Unreal 模块。
pub fn get_module_by_name(conn: &Connection, name: &str) -> anyhow::Result<Value> {
    let name = name.trim();

    if name.is_empty() {
        return Ok(Value::Null);
    }

    let sql = format!(
        r#"
        {}
        SELECT
            m.id,
            sm.text AS name,
            m.type,
            m.scope,
            dp.full_path AS root_path,
            m.build_cs_path,
            m.owner_name,
            m.component_name,
            m.deep_dependencies
        FROM modules m
        JOIN strings sm ON m.name_id = sm.id
        JOIN dir_paths dp ON m.root_directory_id = dp.id
        WHERE sm.text = ?
        LIMIT 1
        "#,
        PATH_CTE
    );

    let result = conn
        .query_row(&sql, [name], module_row_to_json)
        .optional()?;

    Ok(result.unwrap_or(Value::Null))
}

/// Search modules by partial module name.
/// 根据模块名片段搜索 Unreal 模块。
pub fn search_modules(conn: &Connection, part: &str, limit: Option<usize>) -> anyhow::Result<Value> {
    let part = part.trim();

    if part.is_empty() {
        return Ok(json!([]));
    }

    let pattern = format!("%{}%", escape_like(part));
    let limit = limit.unwrap_or(100).min(1000) as i64;

    let sql = format!(
        r#"
        {}
        SELECT
            m.id,
            sm.text AS name,
            m.type,
            m.scope,
            dp.full_path AS root_path,
            m.build_cs_path,
            m.owner_name,
            m.component_name,
            m.deep_dependencies
        FROM modules m
        JOIN strings sm ON m.name_id = sm.id
        JOIN dir_paths dp ON m.root_directory_id = dp.id
        WHERE sm.text LIKE ? ESCAPE '\'
        ORDER BY sm.text
        LIMIT ?
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![pattern, limit], module_row_to_json)?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }

    Ok(json!(results))
}

/// Get module names only.
/// 只获取模块名列表，适合补全或轻量选择器。
pub fn get_module_names(conn: &Connection) -> anyhow::Result<Value> {
    let sql = r#"
        SELECT sm.text AS name
        FROM modules m
        JOIN strings sm ON m.name_id = sm.id
        ORDER BY sm.text
    "#;

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(Value::String(row?));
    }

    Ok(json!(results))
}

/// Convert one SQL row into the JSON shape used by Neovim/UI.
/// 把一行 SQL 查询结果转换成 Neovim/UI 使用的 JSON 结构。
fn module_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "name": row.get::<_, String>(1)?,
        "type": row.get::<_, Option<String>>(2)?,
        "scope": row.get::<_, Option<String>>(3)?,
        "module_root": normalize_path(&row.get::<_, String>(4)?),
        "build_cs_path": row.get::<_, Option<String>>(5)?.map(|p| normalize_path(&p)),
        "owner_name": row.get::<_, Option<String>>(6)?,
        "component_name": row.get::<_, Option<String>>(7)?,
        "deep_dependencies": row.get::<_, Option<String>>(8)?,
    }))
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
