//! Global find search rules.
//! 全局搜索规则。
//!
//! `GlobalFind` is the single backend contract for `:UCore find` / `gf`.
//! Lua sends only a semantic request: `pattern + limit + offset`.
//! This Rust module owns SQL, DB schema details, text scanning, ranking,
//! pagination, and future index changes.
//! `GlobalFind` 是 `:UCore find` / `gf` 的统一后端契约。Lua 只发送
//! `pattern + limit + offset` 语义请求；SQL、数据库结构、文本扫描、
//! 排序、分页以及后续索引变更都由 Rust 侧负责。
//!
//! Live find ranking contract:
//! 1. project class-like results (`class`, `struct`, `enum`, `UCLASS`,
//!    `USTRUCT`, `UENUM`);
//! 2. project file basename/path results;
//! 3. other project symbols such as functions, methods, properties, members;
//! 4. project code text line matches;
//! 5. Engine results appended later and ranked after project results;
//! 6. loose picker-side fuzzy only breaks ties inside the staged results.
//!
//! `FastFind` intentionally omits code text so live search can show class/file
//! results quickly. Lua starts `SearchCodeText` as a separate project-only stage
//! and starts Engine `FastFind` as the final low-priority stage.
//! 实时搜索规则：Project 类结果优先，其次文件名/路径，再是普通 symbol，
//! 然后才是 Project 代码正文；Engine 结果后补并整体排在 Project 后面。
//! `FastFind` 不查正文，正文由 Lua 另起 `SearchCodeText` 阶段追加。

use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection};
use serde_json::{json, Value};
use std::fs;

use crate::db::project_path::PATH_CTE;

/// Search indexed symbols with database-side ranking and pagination.
/// 使用数据库侧排序和分页搜索已索引的 symbol。
///
/// Ranking intentionally favors human "global find" expectations inside the
/// symbol bucket:
/// - exact symbol names first;
/// - then symbol name prefixes;
/// - then continuous symbol substrings;
/// - then owner class / module / path matches;
/// - only after those should picker-side fuzzy matching matter.
///
/// This prevents a query such as `death` from being dominated by loose
/// `d ... e ... a ... t ... h` fuzzy results before real `Death*` symbols.
/// 这里描述的是 symbol 桶内部排序：完全匹配、前缀匹配、连续子串优先，
/// 再看所属 class/module/path，最后才让前端 fuzzy 参与。
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

/// Unified global find for symbols, files, and code text.
/// 统一搜索 symbol、文件名/路径和代码文本。
///
/// Unified find used by non-live callers. Live `gf` uses staged requests
/// (`FastFind`, `SearchCodeText`, then Engine `FastFind`) and the UI applies the
/// final bucket order: Classes > Files > Symbols > Text > Engine. This function
/// still keeps a stable backend order for broad one-shot searches.
/// 非实时入口使用这个统一查询；实时 `gf` 走分阶段请求，并由 UI 应用最终桶排序：
/// Classes > Files > Symbols > Text > Engine。这里仍保留一次性搜索的稳定后端顺序。
pub fn global_find(
    conn: &Connection,
    pattern: &str,
    limit: usize,
    offset: usize,
) -> anyhow::Result<Value> {
    let pattern = pattern.trim();
    let limit = limit.clamp(1, 500);
    let offset = offset.min(1_000_000);

    if pattern.is_empty() {
        return list_symbols(conn, limit, offset);
    }

    let target = offset.saturating_add(limit);
    let mut results = Vec::new();

    extend_json_array(&mut results, search_symbols(conn, pattern, target.max(limit), 0)?);
    extend_json_array(&mut results, search_files_for_global(conn, pattern, target.max(limit))?);

    if results.len() < target {
        extend_json_array(&mut results, search_text_for_global(conn, pattern, target)?);
    }

    dedupe_find_results(&mut results);

    let page = results
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();

    Ok(json!(page))
}

/// Fast first-stage live find.
///
/// This returns only class/symbol rows and file rows. Code text is deliberately
/// excluded because scanning source files can block the first screen. Lua runs
/// `SearchCodeText` in parallel as a project-only append stage, and queries
/// Engine with a separate low-priority `FastFind` request.
/// 实时搜索第一阶段：只返回 class/symbol 和 file，不扫代码正文。代码正文由
/// Lua 并行追加，Engine 另走低优先级 `FastFind`。
pub fn fast_find(
    conn: &Connection,
    pattern: &str,
    limit: usize,
    offset: usize,
) -> anyhow::Result<Value> {
    let pattern = pattern.trim();
    let limit = limit.clamp(1, 500);
    let offset = offset.min(1_000_000);

    if pattern.is_empty() {
        return list_symbols(conn, limit, offset);
    }

    let target = offset.saturating_add(limit);
    let mut results = Vec::new();

    extend_json_array(&mut results, fast_find_symbols(conn, pattern, target.max(limit))?);
    extend_json_array(&mut results, search_files_for_global(conn, pattern, target.max(limit))?);
    dedupe_find_results(&mut results);

    let page = results
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();

    Ok(json!(page))
}

fn fast_find_symbols(conn: &Connection, pattern: &str, limit: usize) -> anyhow::Result<Value> {
    let limit = limit.clamp(1, 500) as i64;
    let query = pattern.to_ascii_lowercase();
    let prefix_query = format!("{}%", escape_like(&query));
    let contains_query = format!("%{}%", escape_like(&query));

    let sql = format!(
        r#"
        {}
        , matched AS (
            SELECT
                sfts.rowid_ref,
                sfts.name,
                sfts.type,
                sfts.class_name,
                CASE
                    WHEN lower(sfts.name) = ? THEN 0
                    WHEN lower(sfts.name) LIKE ? ESCAPE '\' THEN 1
                    WHEN lower(sfts.name) LIKE ? ESCAPE '\' THEN 2
                    WHEN lower(COALESCE(sfts.class_name, '')) LIKE ? ESCAPE '\' THEN 3
                    ELSE 9
                END AS rank
            FROM symbols_fts sfts
            WHERE lower(sfts.name) LIKE ? ESCAPE '\'
               OR lower(COALESCE(sfts.class_name, '')) LIKE ? ESCAPE '\'
            ORDER BY rank, lower(sfts.name) ASC
            LIMIT ?
        )
        SELECT
            matched.name,
            matched.type,
            matched.class_name,
            dp.full_path || '/' || sn.text AS path,
            COALESCE(c.line_number, mem.line_number)
        FROM matched
        LEFT JOIN classes c
            ON c.id = matched.rowid_ref
           AND {}
        LEFT JOIN members mem
            ON mem.id = matched.rowid_ref
           AND NOT ({})
        JOIN files f ON f.id = COALESCE(c.file_id, mem.file_id)
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        ORDER BY matched.rank, lower(matched.name) ASC, path ASC
        "#,
        PATH_CTE,
        class_symbol_predicate("matched"),
        class_symbol_predicate("matched")
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            query,
            prefix_query,
            contains_query,
            contains_query,
            contains_query,
            contains_query,
            limit
        ],
        |row| {
            Ok(json!({
                "name": row.get::<_, String>(0)?,
                "type": row.get::<_, String>(1)?,
                "class_name": row.get::<_, Option<String>>(2)?,
                "path": normalize_path(&row.get::<_, String>(3)?),
                "line": row.get::<_, Option<i64>>(4)?,
            }))
        },
    )?;

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

fn extend_json_array(target: &mut Vec<Value>, value: Value) {
    if let Some(values) = value.as_array() {
        target.extend(values.iter().cloned());
    }
}

fn dedupe_find_results(results: &mut Vec<Value>) {
    let mut seen = std::collections::HashSet::new();
    results.retain(|item| {
        let path = item
            .get("path")
            .or_else(|| item.get("file_path"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let line = item
            .get("line")
            .or_else(|| item.get("line_number"))
            .and_then(Value::as_i64)
            .unwrap_or(1);
        let name = item
            .get("name")
            .or_else(|| item.get("symbol_name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = item
            .get("type")
            .or_else(|| item.get("symbol_type"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        seen.insert(format!("{kind}\t{path}\t{line}\t{name}"))
    });
}

fn search_files_for_global(
    conn: &Connection,
    pattern: &str,
    limit: usize,
) -> anyhow::Result<Value> {
    let limit = limit.clamp(1, 500) as i64;
    let query = pattern.to_ascii_lowercase();
    let prefix_query = format!("{}%", escape_like(&query));
    let contains_query = format!("%{}%", escape_like(&query));

    let sql = format!(
        r#"
        {}
        SELECT
            sn.text AS filename,
            dp.full_path || '/' || sn.text AS path,
            sm.text AS module_name,
            rd.full_path AS module_root
        FROM files f
        JOIN strings sn ON f.filename_id = sn.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        LEFT JOIN modules m ON f.module_id = m.id
        LEFT JOIN strings sm ON m.name_id = sm.id
        LEFT JOIN dir_paths rd ON m.root_directory_id = rd.id
        WHERE lower(sn.text) LIKE ? ESCAPE '\'
           OR lower(dp.full_path || '/' || sn.text) LIKE ? ESCAPE '\'
        ORDER BY
            CASE
                WHEN lower(sn.text) = ? THEN 0
                WHEN lower(sn.text) LIKE ? ESCAPE '\' THEN 1
                WHEN lower(sn.text) LIKE ? ESCAPE '\' THEN 2
                WHEN lower(dp.full_path || '/' || sn.text) LIKE ? ESCAPE '\' THEN 3
                ELSE 9
            END,
            lower(sn.text) ASC,
            lower(dp.full_path || '/' || sn.text) ASC
        LIMIT ?
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            contains_query,
            contains_query,
            query,
            prefix_query,
            contains_query,
            contains_query,
            limit
        ],
        |row| {
            Ok(json!({
                "name": row.get::<_, String>(0)?,
                "type": "file",
                "path": normalize_path(&row.get::<_, String>(1)?),
                "line": 1,
                "module_name": row.get::<_, Option<String>>(2)?,
                "module_root": row.get::<_, Option<String>>(3)?.map(|p| normalize_path(&p)),
            }))
        },
    )?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }

    Ok(json!(results))
}

fn search_text_for_global(
    conn: &Connection,
    pattern: &str,
    limit: usize,
) -> anyhow::Result<Value> {
    search_code_text(conn, pattern, limit, 0)
}

pub fn search_code_text(
    conn: &Connection,
    pattern: &str,
    limit: usize,
    offset: usize,
) -> anyhow::Result<Value> {
    let needle = pattern.to_ascii_lowercase();
    if needle.is_empty() {
        return Ok(json!([]));
    }

    let limit = limit.clamp(1, 500);
    let offset = offset.min(1_000_000);
    let paths = indexed_text_file_paths(conn)?;
    let mut results = Vec::new();
    let mut skipped = 0usize;

    for path in paths {
        if results.len() >= limit {
            break;
        }

        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };

        for (line_index, line) in content.lines().enumerate() {
            if line.to_ascii_lowercase().contains(&needle) {
                if skipped < offset {
                    skipped += 1;
                    continue;
                }

                results.push(json!({
                    "name": pattern,
                    "type": "text",
                    "path": normalize_path(&path),
                    "line": (line_index + 1) as i64,
                    "text": line.trim(),
                }));

                if results.len() >= limit {
                    break;
                }
            }
        }
    }

    Ok(json!(results))
}

fn indexed_text_file_paths(conn: &Connection) -> anyhow::Result<Vec<String>> {
    let sql = format!(
        r#"
        {}
        SELECT dp.full_path || '/' || sn.text AS path
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sn ON f.filename_id = sn.id
        WHERE lower(f.extension) IN (
            'h', 'hh', 'hpp', 'hxx',
            'c', 'cc', 'cpp', 'cxx',
            'inl', 'ipp',
            'cs', 'ini', 'json', 'uproject', 'uplugin'
        )
        ORDER BY path
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut paths = Vec::new();

    for row in rows {
        paths.push(normalize_path(&row?));
    }

    Ok(paths)
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
