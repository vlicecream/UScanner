use std::collections::HashMap;
use std::path::{Component, Path};

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

/// Path separator used by the index database.
/// 索引数据库内部统一使用的路径分隔符。
const DB_PATH_SEPARATOR: &str = "/";

/// Get or create a normalized directory tree and return the deepest directory id.
/// 获取或创建规范化目录树，并返回最深层目录的 id。
///
/// The directory identity is `(parent_id, name_id)`, not just the directory name.
/// 目录的唯一身份是 `(parent_id, name_id)`，不是单独的目录名。
pub fn get_or_create_directory(
    tx: &Transaction,
    str_cache: &mut HashMap<String, i64>,
    dir_cache: &mut HashMap<(Option<i64>, i64), i64>,
    path: &Path,
) -> rusqlite::Result<i64> {
    let mut current_parent_id: Option<i64> = None;

    for name in normalize_path_components(path) {
        let name_id = crate::db::get_or_create_string(tx, str_cache, &name)?;
        let cache_key = (current_parent_id, name_id);

        if let Some(&directory_id) = dir_cache.get(&cache_key) {
            current_parent_id = Some(directory_id);
            continue;
        }

        let directory_id = find_directory(tx, current_parent_id, name_id)?
            .unwrap_or_else(|| {
                insert_directory(tx, current_parent_id, name_id)
                    .expect("failed to insert directory")
            });

        dir_cache.insert(cache_key, directory_id);
        current_parent_id = Some(directory_id);
    }

    Ok(current_parent_id.unwrap_or(0))
}

/// Restore a full path from directory_id and filename_id.
/// 根据 directory_id 和 filename_id 还原完整路径。
pub fn get_full_path(
    conn: &Connection,
    directory_id: i64,
    filename_id: i64,
) -> anyhow::Result<String> {
    let directory = get_directory_path(conn, directory_id)
        .with_context(|| format!("failed to restore directory path for id {}", directory_id))?;

    let filename: String = conn
        .query_row(
            "SELECT text FROM strings WHERE id = ?",
            [filename_id],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to read filename string id {}", filename_id))?;

    Ok(join_db_path(&directory, &filename))
}

/// Common CTE for building full directory paths inside SQL queries.
/// 在 SQL 查询中构建完整目录路径的通用 CTE。
///
/// This CTE creates:
/// `dir_paths(id, full_path)`
///
/// 这个 CTE 会生成：
/// `dir_paths(id, full_path)`
pub const PATH_CTE: &str = r#"
    WITH RECURSIVE dir_paths(id, full_path) AS (
        SELECT
            d.id,
            s.text
        FROM directories d
        JOIN strings s ON d.name_id = s.id
        WHERE d.parent_id IS NULL

        UNION ALL

        SELECT
            d.id,
            CASE
                WHEN dp.full_path = '/'
                    THEN '/' || s.text
                WHEN s.text = '/'
                    THEN dp.full_path || '/'
                ELSE dp.full_path || '/' || s.text
            END
        FROM directories d
        JOIN dir_paths dp ON d.parent_id = dp.id
        JOIN strings s ON d.name_id = s.id
    )
"#;

/// Common SELECT fragment for returning files with their restored paths.
/// 返回 files 表记录及其完整路径的通用 SELECT 片段。
pub const FILE_PATH_SELECT: &str = r#"
    SELECT
        f.*,
        CASE
            WHEN dp.full_path = ''
                THEN sn.text
            WHEN dp.full_path = '/'
                THEN '/' || sn.text
            WHEN substr(dp.full_path, -1) = '/'
                THEN dp.full_path || sn.text
            ELSE dp.full_path || '/' || sn.text
        END AS path
    FROM files f
    JOIN dir_paths dp ON f.directory_id = dp.id
    JOIN strings sn ON f.filename_id = sn.id
"#;

/// Normalize a filesystem path into database directory components.
/// 把文件系统路径规范化成数据库目录组件。
///
/// Examples:
/// - `C:\Project\Source` -> `["C:", "/", "Project", "Source"]`
/// - `/home/me/project` -> `["/", "home", "me", "project"]`
///
/// 示例：
/// - `C:\Project\Source` -> `["C:", "/", "Project", "Source"]`
/// - `/home/me/project` -> `["/", "home", "me", "project"]`
fn normalize_path_components(path: &Path) -> Vec<String> {
    let mut parts = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                parts.push(prefix.as_os_str().to_string_lossy().to_string());
            }
            Component::RootDir => {
                parts.push(DB_PATH_SEPARATOR.to_string());
            }
            Component::Normal(name) => {
                let text = name.to_string_lossy().to_string();

                if !text.is_empty() {
                    parts.push(text);
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                // Keep normalized directory storage simple.
                // 保持目录存储简单：调用方应传入 canonicalized path。
            }
        }
    }

    parts
}

/// Find an existing directory by `(parent_id, name_id)`.
/// 通过 `(parent_id, name_id)` 查找已有目录。
fn find_directory(
    tx: &Transaction,
    parent_id: Option<i64>,
    name_id: i64,
) -> rusqlite::Result<Option<i64>> {
    tx.query_row(
        r#"
        SELECT id
        FROM directories
        WHERE
            (
                (?1 IS NULL AND parent_id IS NULL)
                OR parent_id = ?1
            )
            AND name_id = ?2
        "#,
        params![parent_id, name_id],
        |row| row.get(0),
    )
    .optional()
}

/// Insert a new directory row.
/// 插入新的目录记录。
fn insert_directory(
    tx: &Transaction,
    parent_id: Option<i64>,
    name_id: i64,
) -> rusqlite::Result<i64> {
    tx.execute(
        "INSERT INTO directories (parent_id, name_id) VALUES (?1, ?2)",
        params![parent_id, name_id],
    )?;

    Ok(tx.last_insert_rowid())
}

/// Restore a directory path from directory_id.
/// 根据 directory_id 还原目录路径。
pub(crate) fn get_directory_path(conn: &Connection, directory_id: i64) -> anyhow::Result<String> {
    if directory_id == 0 {
        return Ok(String::new());
    }

    let mut stmt = conn.prepare(
        r#"
        WITH RECURSIVE path_builder(id, parent_id, name_id, depth) AS (
            SELECT id, parent_id, name_id, 0
            FROM directories
            WHERE id = ?1

            UNION ALL

            SELECT d.id, d.parent_id, d.name_id, pb.depth + 1
            FROM directories d
            JOIN path_builder pb ON d.id = pb.parent_id
        )
        SELECT s.text
        FROM path_builder pb
        JOIN strings s ON pb.name_id = s.id
        ORDER BY pb.depth DESC
        "#,
    )?;

    let mut rows = stmt.query([directory_id])?;
    let mut parts = Vec::new();

    while let Some(row) = rows.next()? {
        parts.push(row.get::<_, String>(0)?);
    }

    Ok(normalize_db_path_parts(&parts))
}

/// Join directory path and filename using the database path format.
/// 用数据库路径格式拼接目录和文件名。
fn join_db_path(directory: &str, filename: &str) -> String {
    if directory.is_empty() {
        return normalize_slashes(filename);
    }

    if directory == DB_PATH_SEPARATOR {
        return format!("/{}", filename);
    }

    if directory.ends_with(DB_PATH_SEPARATOR) {
        return normalize_slashes(&format!("{}{}", directory, filename));
    }

    normalize_slashes(&format!("{}/{}", directory, filename))
}

/// Convert restored path parts into a normalized database path.
/// 把还原出的路径组件转换成规范化数据库路径。
fn normalize_db_path_parts(parts: &[String]) -> String {
    if parts.is_empty() {
        return String::new();
    }

    let mut output = String::new();

    for part in parts {
        if part == DB_PATH_SEPARATOR {
            if output.is_empty() {
                output.push_str(DB_PATH_SEPARATOR);
            } else if !output.ends_with(DB_PATH_SEPARATOR) {
                output.push_str(DB_PATH_SEPARATOR);
            }

            continue;
        }

        if output.is_empty() {
            output.push_str(part);
        } else if output.ends_with(DB_PATH_SEPARATOR) {
            output.push_str(part);
        } else {
            output.push_str(DB_PATH_SEPARATOR);
            output.push_str(part);
        }
    }

    normalize_slashes(&output)
}

/// Normalize Windows separators into database separators.
/// 把 Windows 分隔符规范化为数据库分隔符。
fn normalize_slashes(path: &str) -> String {
    let mut normalized = path.replace('\\', DB_PATH_SEPARATOR);

    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }

    normalized
}
