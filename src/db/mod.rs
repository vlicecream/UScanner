pub mod project_path;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use tracing::info;

use crate::types::{ParseResult, ProgressReporter};

/// Main SQLite schema version.
/// 主数据库 schema 版本。
///
/// Increment this when table structures, indexes, or stored data semantics change.
/// 当表结构、索引或存储语义变化时递增。
pub const DB_VERSION: i32 = 19;

/// Completion cache version.
/// 补全缓存版本。
///
/// Increment this when completion logic changes but the main DB schema does not.
/// 当补全逻辑变化但主数据库 schema 不变时递增。
pub const COMPLETION_CACHE_VERSION: i32 = 4;

/// SQLite busy timeout for normal operations.
/// 普通数据库操作的 busy timeout。
const DB_BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// SQLite busy timeout for bulk writes.
/// 批量写入时的 busy timeout。
const DB_BULK_BUSY_TIMEOUT: Duration = Duration::from_millis(60_000);

/// Ensure the on-disk database matches the current schema version.
/// 确保磁盘数据库版本和当前 schema 版本一致。
///
/// Returns true when the database was newly initialized or rebuilt.
/// 如果数据库被新建或重建，返回 true。
pub fn ensure_correct_version(db_path: &str) -> anyhow::Result<bool> {
    let db_exists = Path::new(db_path).exists();

    if db_exists && database_version_matches(db_path)? {
        return Ok(false);
    }

    if db_exists {
        info!(
            "DB version mismatch or missing. Re-initializing {} with version {}.",
            db_path, DB_VERSION
        );

        std::fs::remove_file(db_path)
            .with_context(|| format!("failed to remove old database {}", db_path))?;
    }

    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open database {}", db_path))?;
    init_db(&conn)?;

    Ok(true)
}

/// Check whether an existing database has the expected schema version.
/// 检查现有数据库是否是预期 schema 版本。
fn database_version_matches(db_path: &str) -> anyhow::Result<bool> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open database {}", db_path))?;

    let version = conn
        .query_row(
            "SELECT value FROM project_meta WHERE key = 'db_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse::<i32>().ok());

    Ok(version == Some(DB_VERSION))
}

/// Initialize all database tables, indexes, and metadata.
/// 初始化所有数据库表、索引和元数据。
pub fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(DB_BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;

    create_tables(conn)?;
    create_views(conn)?;
    create_indices(conn)?;

    conn.execute(
        "INSERT OR REPLACE INTO project_meta (key, value) VALUES ('db_version', ?1)",
        [DB_VERSION.to_string()],
    )?;

    Ok(())
}

/// Create all tables used by UCore's project index.
/// 创建 UCore 项目索引用到的全部表。
fn create_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS strings (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            text TEXT NOT NULL UNIQUE
        );

        CREATE TABLE IF NOT EXISTS directories (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            parent_id INTEGER,
            name_id INTEGER NOT NULL,
            UNIQUE(parent_id, name_id),
            FOREIGN KEY(parent_id) REFERENCES directories(id) ON DELETE CASCADE,
            FOREIGN KEY(name_id) REFERENCES strings(id)
        );

        CREATE TABLE IF NOT EXISTS modules (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name_id INTEGER NOT NULL,
            type TEXT,
            scope TEXT,
            root_directory_id INTEGER NOT NULL,
            build_cs_path TEXT,
            owner_name TEXT,
            component_name TEXT,
            deep_dependencies TEXT,
            UNIQUE(name_id, root_directory_id),
            FOREIGN KEY(name_id) REFERENCES strings(id),
            FOREIGN KEY(root_directory_id) REFERENCES directories(id)
        );

        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            directory_id INTEGER NOT NULL,
            filename_id INTEGER NOT NULL,
            extension TEXT,
            mtime INTEGER,
            module_id INTEGER,
            is_header INTEGER DEFAULT 0,
            file_hash TEXT,
            UNIQUE(directory_id, filename_id),
            FOREIGN KEY(directory_id) REFERENCES directories(id) ON DELETE CASCADE,
            FOREIGN KEY(filename_id) REFERENCES strings(id),
            FOREIGN KEY(module_id) REFERENCES modules(id)
        );

        CREATE TABLE IF NOT EXISTS classes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name_id INTEGER NOT NULL,
            namespace_id INTEGER,
            base_class_id INTEGER,
            file_id INTEGER,
            line_number INTEGER,
            end_line_number INTEGER,
            symbol_type TEXT DEFAULT 'class',
            FOREIGN KEY(name_id) REFERENCES strings(id),
            FOREIGN KEY(namespace_id) REFERENCES strings(id),
            FOREIGN KEY(base_class_id) REFERENCES strings(id),
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS members (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            class_id INTEGER NOT NULL,
            name_id INTEGER NOT NULL,
            type_id INTEGER NOT NULL,
            flags TEXT,
            access TEXT,
            detail TEXT,
            return_type_id INTEGER,
            is_static INTEGER,
            line_number INTEGER,
            file_id INTEGER,
            FOREIGN KEY(class_id) REFERENCES classes(id) ON DELETE CASCADE,
            FOREIGN KEY(name_id) REFERENCES strings(id),
            FOREIGN KEY(type_id) REFERENCES strings(id),
            FOREIGN KEY(return_type_id) REFERENCES strings(id),
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS enum_values (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            enum_id INTEGER NOT NULL,
            name_id INTEGER NOT NULL,
            line_number INTEGER,
            file_id INTEGER,
            FOREIGN KEY(enum_id) REFERENCES classes(id) ON DELETE CASCADE,
            FOREIGN KEY(name_id) REFERENCES strings(id),
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS inheritance (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            child_id INTEGER NOT NULL,
            parent_name_id INTEGER NOT NULL,
            parent_class_id INTEGER,
            FOREIGN KEY(child_id) REFERENCES classes(id) ON DELETE CASCADE,
            FOREIGN KEY(parent_name_id) REFERENCES strings(id),
            FOREIGN KEY(parent_class_id) REFERENCES classes(id) ON DELETE SET NULL
        );

        CREATE TABLE IF NOT EXISTS symbol_calls (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_id INTEGER NOT NULL,
            line INTEGER NOT NULL,
            name_id INTEGER NOT NULL,
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE,
            FOREIGN KEY(name_id) REFERENCES strings(id)
        );

        CREATE TABLE IF NOT EXISTS file_includes (
            file_id INTEGER NOT NULL,
            include_path_id INTEGER NOT NULL,
            base_filename_id INTEGER NOT NULL,
            resolved_file_id INTEGER,
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE,
            FOREIGN KEY(include_path_id) REFERENCES strings(id),
            FOREIGN KEY(base_filename_id) REFERENCES strings(id),
            FOREIGN KEY(resolved_file_id) REFERENCES files(id) ON DELETE SET NULL
        );

        CREATE TABLE IF NOT EXISTS components (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            display_name TEXT,
            type TEXT,
            owner_name TEXT,
            root_path TEXT,
            uplugin_path TEXT,
            uproject_path TEXT,
            engine_association TEXT
        );

        CREATE TABLE IF NOT EXISTS project_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS persistent_cache (
            key TEXT PRIMARY KEY,
            value BLOB NOT NULL,
            hit_count INTEGER DEFAULT 1,
            last_used INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS cache_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )?;

    let _ = conn.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
            name,
            type,
            class_name UNINDEXED,
            rowid_ref UNINDEXED
        )",
        [],
    );

    Ok(())
}

/// Create query helper views.
/// 创建查询辅助视图。
fn create_views(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE VIEW IF NOT EXISTS dir_paths AS
        WITH RECURSIVE paths(id, full_path) AS (
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
                    WHEN paths.full_path = '/'
                        THEN '/' || s.text
                    WHEN s.text = '/'
                        THEN paths.full_path || '/'
                    ELSE paths.full_path || '/' || s.text
                END
            FROM directories d
            JOIN paths ON d.parent_id = paths.id
            JOIN strings s ON d.name_id = s.id
        )
        SELECT id, full_path
        FROM paths;
        "#,
    )?;

    Ok(())
}

/// Create query indexes.
/// 创建查询索引。
fn create_indices(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_strings_text ON strings(text);
        CREATE INDEX IF NOT EXISTS idx_directories_parent ON directories(parent_id);

        CREATE INDEX IF NOT EXISTS idx_files_filename_id ON files(filename_id);
        CREATE INDEX IF NOT EXISTS idx_files_dir_id ON files(directory_id);
        CREATE INDEX IF NOT EXISTS idx_files_module_id ON files(module_id);

        CREATE INDEX IF NOT EXISTS idx_classes_covering
            ON classes(name_id, file_id, line_number, symbol_type);
        CREATE INDEX IF NOT EXISTS idx_classes_file_id ON classes(file_id);

        CREATE INDEX IF NOT EXISTS idx_members_name_id ON members(name_id);
        CREATE INDEX IF NOT EXISTS idx_members_file_id ON members(file_id);
        CREATE INDEX IF NOT EXISTS idx_members_class_id ON members(class_id);

        CREATE INDEX IF NOT EXISTS idx_symbol_calls_file_id ON symbol_calls(file_id);
        CREATE INDEX IF NOT EXISTS idx_symbol_calls_name_id ON symbol_calls(name_id);

        CREATE INDEX IF NOT EXISTS idx_file_includes_file_id ON file_includes(file_id);
        CREATE INDEX IF NOT EXISTS idx_file_includes_resolved_id ON file_includes(resolved_file_id);
        CREATE INDEX IF NOT EXISTS idx_file_includes_base_name ON file_includes(base_filename_id);

        CREATE INDEX IF NOT EXISTS idx_cache_last_used ON persistent_cache(last_used);
        "#,
    )?;

    Ok(())
}

/// Drop indexes before large insert batches.
/// 大批量插入前删除索引以提升写入速度。
fn drop_indices(conn: &Connection) -> rusqlite::Result<()> {
    let indices = [
        "idx_strings_text",
        "idx_directories_parent",
        "idx_files_filename_id",
        "idx_files_dir_id",
        "idx_files_module_id",
        "idx_classes_covering",
        "idx_classes_file_id",
        "idx_members_name_id",
        "idx_members_file_id",
        "idx_members_class_id",
        "idx_symbol_calls_file_id",
        "idx_symbol_calls_name_id",
        "idx_file_includes_file_id",
        "idx_file_includes_resolved_id",
        "idx_file_includes_base_name",
        "idx_cache_last_used",
    ];

    for index_name in indices {
        let sql = format!("DROP INDEX IF EXISTS {}", index_name);
        let _ = conn.execute(&sql, []);
    }

    Ok(())
}

/// Get or create one interned string id.
/// 获取或创建字符串池中的字符串 id。
pub fn get_or_create_string(
    tx: &rusqlite::Transaction,
    cache: &mut HashMap<String, i64>,
    text: &str,
) -> rusqlite::Result<i64> {
    let text = text.trim();

    if let Some(&id) = cache.get(text) {
        return Ok(id);
    }

    let existing = tx
        .query_row(
            "SELECT id FROM strings WHERE text = ?1",
            [text],
            |row| row.get(0),
        )
        .optional()?;

    let id = match existing {
        Some(id) => id,
        None => {
            tx.execute("INSERT INTO strings (text) VALUES (?1)", [text])?;
            tx.last_insert_rowid()
        }
    };

    cache.insert(text.to_string(), id);
    Ok(id)
}

/// Save parser results into SQLite.
/// 把解析结果保存到 SQLite。
pub fn save_to_db(
    conn: &mut Connection,
    results: &[ParseResult],
    reporter: Arc<dyn ProgressReporter>,
) -> anyhow::Result<()> {
    init_db(conn)?;

    prepare_bulk_write(conn)?;
    reporter.report("db_sync", 0, 100, "Dropping indices for faster insertion...");
    drop_indices(conn)?;

    let total = results.len();
    reporter.report("db_sync", 0, total, &format!("Saving results (0/{})", total));

    let tx = conn.transaction()?;
    let mut string_cache: HashMap<String, i64> = HashMap::new();
    let mut dir_cache: HashMap<(Option<i64>, i64), i64> = HashMap::new();

    {
        let mut stmt_delete_file =
            tx.prepare("DELETE FROM files WHERE directory_id = ?1 AND filename_id = ?2")?;

        let mut stmt_file = tx.prepare(
            "INSERT INTO files
             (directory_id, filename_id, extension, mtime, file_hash, module_id, is_header)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;

        let mut stmt_class = tx.prepare(
            "INSERT INTO classes
             (name_id, namespace_id, base_class_id, file_id, line_number, symbol_type, end_line_number)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;

        let mut stmt_inheritance = tx.prepare(
            "INSERT INTO inheritance (child_id, parent_name_id)
             VALUES (?1, ?2)",
        )?;

        let mut stmt_enum = tx.prepare(
            "INSERT INTO enum_values (enum_id, name_id, line_number, file_id)
             VALUES (?1, ?2, ?3, ?4)",
        )?;

        let mut stmt_member = tx.prepare(
            "INSERT INTO members
             (class_id, name_id, type_id, flags, access, detail, return_type_id, is_static, line_number, file_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;

        let mut stmt_call = tx.prepare(
            "INSERT INTO symbol_calls (file_id, line, name_id)
             VALUES (?1, ?2, ?3)",
        )?;

        let mut stmt_fts = tx.prepare(
            "INSERT INTO symbols_fts (name, type, class_name, rowid_ref)
             VALUES (?1, ?2, ?3, ?4)",
        )?;

        let mut stmt_include = tx.prepare(
            "INSERT INTO file_includes (file_id, include_path_id, base_filename_id)
             VALUES (?1, ?2, ?3)",
        )?;

        let mut last_reported_percent = 0usize;

        for (index, result) in results.iter().enumerate() {
            let current = index + 1;
            let percent = progress_percent(current, total);

            if current == total || percent > last_reported_percent {
                last_reported_percent = percent;
                reporter.report(
                    "db_sync",
                    current,
                    total,
                    &format!("Saving results ({}/{})", current, total),
                );
            }

            if result.status != "parsed" {
                continue;
            }

            let Some(data) = &result.data else {
                continue;
            };

            let path_obj = Path::new(&result.path);
            let parent_dir = path_obj.parent().unwrap_or_else(|| Path::new(""));
            let filename = path_obj
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            let extension = path_obj
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();

            let dir_id = project_path::get_or_create_directory(
                &tx,
                &mut string_cache,
                &mut dir_cache,
                parent_dir,
            )?;
            let filename_id = get_or_create_string(&tx, &mut string_cache, filename)?;

            let _ = stmt_delete_file.execute(params![dir_id, filename_id]);

            stmt_file.execute(params![
                dir_id,
                filename_id,
                extension,
                result.mtime as i64,
                data.new_hash,
                result.module_id,
                is_header_extension(&extension) as i32,
            ])?;

            let file_id = tx.last_insert_rowid();

            save_classes(
                &tx,
                &mut string_cache,
                &mut stmt_class,
                &mut stmt_inheritance,
                &mut stmt_enum,
                &mut stmt_member,
                &mut stmt_fts,
                file_id,
                &data.classes,
            )?;

            save_calls(
                &tx,
                &mut string_cache,
                &mut stmt_call,
                file_id,
                &data.calls,
            )?;

            save_includes(
                &tx,
                &mut string_cache,
                &mut stmt_include,
                file_id,
                &data.includes,
            )?;
        }
    }

    tx.commit()?;

    finalize_bulk_write(conn, reporter)?;
    Ok(())
}

/// Convert item progress into a 0-100 percentage.
/// 将条目进度换算成 0-100 百分比。
fn progress_percent(current: usize, total: usize) -> usize {
    if total == 0 {
        return 100;
    }

    (current * 100 / total).min(100)
}

/// Configure SQLite for fast bulk insertion.
/// 配置 SQLite 以提升批量写入性能。
fn prepare_bulk_write(conn: &Connection) -> anyhow::Result<()> {
    conn.busy_timeout(DB_BULK_BUSY_TIMEOUT)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "OFF")?;
    conn.pragma_update(None, "cache_size", "-800000")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.execute("PRAGMA foreign_keys = OFF", [])?;
    Ok(())
}

/// Restore indexes, resolve links, and optimize after bulk insertion.
/// 批量写入后恢复索引、解析关系并优化数据库。
fn finalize_bulk_write(
    conn: &mut Connection,
    reporter: Arc<dyn ProgressReporter>,
) -> anyhow::Result<()> {
    reporter.report(
        "finalizing",
        70,
        100,
        "Re-creating indices (this may take a while)...",
    );
    create_indices(conn)?;

    conn.execute("PRAGMA foreign_keys = ON", [])?;

    reporter.report("finalizing", 80, 100, "Optimizing inheritance graph...");
    resolve_inheritance(conn)?;

    reporter.report("finalizing", 85, 100, "Resolving file includes...");
    resolve_file_includes_by_path(conn)?;

    reporter.report("finalizing", 95, 100, "Vacuuming and optimizing...");
    conn.execute("PRAGMA optimize", [])?;

    Ok(())
}

/// Save classes and their members.
/// 保存类及其成员。
#[allow(clippy::too_many_arguments)]
fn save_classes(
    tx: &rusqlite::Transaction,
    string_cache: &mut HashMap<String, i64>,
    stmt_class: &mut rusqlite::Statement,
    stmt_inheritance: &mut rusqlite::Statement,
    stmt_enum: &mut rusqlite::Statement,
    stmt_member: &mut rusqlite::Statement,
    stmt_fts: &mut rusqlite::Statement,
    file_id: i64,
    classes: &[crate::types::ClassInfo],
) -> anyhow::Result<()> {
    for class_info in classes {
        let class_name_id = get_or_create_string(tx, string_cache, &class_info.class_name)?;
        let namespace_id = match &class_info.namespace {
            Some(namespace) => Some(get_or_create_string(tx, string_cache, namespace)?),
            None => None,
        };
        let base_class_id = match class_info.base_classes.first() {
            Some(base_class) => Some(get_or_create_string(tx, string_cache, base_class)?),
            None => None,
        };

        stmt_class.execute(params![
            class_name_id,
            namespace_id,
            base_class_id,
            file_id,
            class_info.line as i64,
            class_info.symbol_type,
            class_info.end_line as i64,
        ])?;

        let class_row_id = tx.last_insert_rowid();

        stmt_fts.execute(params![
            class_info.class_name,
            class_info.symbol_type,
            class_info.class_name,
            class_row_id,
        ])?;

        for parent in &class_info.base_classes {
            let parent_name_id = get_or_create_string(tx, string_cache, parent)?;
            stmt_inheritance.execute(params![class_row_id, parent_name_id])?;
        }

        save_members(
            tx,
            string_cache,
            stmt_enum,
            stmt_member,
            stmt_fts,
            file_id,
            class_row_id,
            class_info,
        )?;
    }

    Ok(())
}

/// Save members for one class.
/// 保存单个类的成员。
#[allow(clippy::too_many_arguments)]
fn save_members(
    tx: &rusqlite::Transaction,
    string_cache: &mut HashMap<String, i64>,
    stmt_enum: &mut rusqlite::Statement,
    stmt_member: &mut rusqlite::Statement,
    stmt_fts: &mut rusqlite::Statement,
    file_id: i64,
    class_row_id: i64,
    class_info: &crate::types::ClassInfo,
) -> anyhow::Result<()> {
    for member in &class_info.members {
        let member_name_id = get_or_create_string(tx, string_cache, &member.name)?;

        if member.mem_type == "enum_item" {
            stmt_enum.execute(params![
                class_row_id,
                member_name_id,
                member.line as i64,
                file_id,
            ])?;
            continue;
        }

        let type_id = get_or_create_string(tx, string_cache, &member.mem_type)?;
        let return_type_id = match &member.return_type {
            Some(return_type) => Some(get_or_create_string(tx, string_cache, return_type)?),
            None => None,
        };

        stmt_member.execute(params![
            class_row_id,
            member_name_id,
            type_id,
            member.flags,
            member.access,
            member.detail,
            return_type_id,
            member.flags.contains("static") as i32,
            member.line as i64,
            file_id,
        ])?;

        stmt_fts.execute(params![
            member.name,
            member.mem_type,
            class_info.class_name,
            tx.last_insert_rowid(),
        ])?;
    }

    Ok(())
}

/// Save function/member calls.
/// 保存函数和成员调用。
fn save_calls(
    tx: &rusqlite::Transaction,
    string_cache: &mut HashMap<String, i64>,
    stmt_call: &mut rusqlite::Statement,
    file_id: i64,
    calls: &[crate::types::CallInfo],
) -> anyhow::Result<()> {
    for call in calls {
        let name_id = get_or_create_string(tx, string_cache, &call.name)?;
        stmt_call.execute(params![file_id, call.line as i64, name_id])?;
    }

    Ok(())
}

/// Save include relationships.
/// 保存 include 关系。
fn save_includes(
    tx: &rusqlite::Transaction,
    string_cache: &mut HashMap<String, i64>,
    stmt_include: &mut rusqlite::Statement,
    file_id: i64,
    includes: &[String],
) -> anyhow::Result<()> {
    for include_path in includes {
        let include_path_id = get_or_create_string(tx, string_cache, include_path)?;
        let base_filename = Path::new(include_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(include_path);
        let base_filename_id = get_or_create_string(tx, string_cache, base_filename)?;

        stmt_include.execute(params![
            file_id,
            include_path_id,
            base_filename_id,
        ])?;
    }

    Ok(())
}

/// Resolve inheritance rows to actual class ids when possible.
/// 尽量把继承关系解析到真实 class id。
fn resolve_inheritance(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        r#"
        UPDATE inheritance
        SET parent_class_id = (
            SELECT c.id
            FROM classes c
            WHERE c.name_id = inheritance.parent_name_id
            LIMIT 1
        )
        WHERE parent_class_id IS NULL
        "#,
        [],
    )?;

    Ok(())
}

/// Resolve includes when the included base filename is unique.
/// 当 include 的文件名唯一时，解析到具体文件。
fn resolve_file_includes_by_path(conn: &mut Connection) -> anyhow::Result<()> {
    conn.execute(
        r#"
        UPDATE file_includes
        SET resolved_file_id = (
            SELECT f.id
            FROM files f
            WHERE f.filename_id = file_includes.base_filename_id
            LIMIT 1
        )
        WHERE resolved_file_id IS NULL
          AND (
            SELECT COUNT(*)
            FROM files f
            WHERE f.filename_id = file_includes.base_filename_id
          ) = 1
        "#,
        [],
    )?;

    Ok(())
}

/// Register or update one Unreal module.
/// 注册或更新 Unreal 模块。
pub fn register_module(
    conn: &Connection,
    name: &str,
    root_path: &str,
    module_type: &str,
    scope: &str,
) -> anyhow::Result<i64> {
    let tx = conn.unchecked_transaction()?;
    let mut string_cache = HashMap::new();
    let mut dir_cache = HashMap::new();

    let name_id = get_or_create_string(&tx, &mut string_cache, name)?;
    let root_dir_id = project_path::get_or_create_directory(
        &tx,
        &mut string_cache,
        &mut dir_cache,
        Path::new(root_path),
    )?;

    tx.execute(
        "INSERT OR REPLACE INTO modules (name_id, root_directory_id, type, scope)
         VALUES (?1, ?2, ?3, ?4)",
        params![name_id, root_dir_id, module_type, scope],
    )?;

    let module_id = tx.last_insert_rowid();
    tx.commit()?;

    Ok(module_id)
}

/// Find the best module for one file path by longest root prefix.
/// 通过最长 root 前缀匹配文件所属模块。
pub fn get_module_id_for_path(
    conn: &Connection,
    file_path: &str,
) -> anyhow::Result<Option<i64>> {
    let mut stmt = conn.prepare("SELECT id, root_directory_id FROM modules")?;
    let mut rows = stmt.query([])?;

    let file_path_norm = normalize_path_for_compare(file_path);
    let mut best_id = None;
    let mut best_len = 0;

    while let Some(row) = rows.next()? {
        let module_id: i64 = row.get(0)?;
        let root_directory_id: i64 = row.get(1)?;

        let root_path = project_path::get_directory_path(conn, root_directory_id)?;
        let root_path_norm = normalize_path_for_compare(&root_path);

        if file_path_norm.starts_with(&root_path_norm) && root_path_norm.len() > best_len {
            best_id = Some(module_id);
            best_len = root_path_norm.len();
        }
    }

    Ok(best_id)
}

/// Return registered Unreal components as JSON.
/// 以 JSON 返回已注册的 Unreal components。
pub fn get_components(conn: &Connection) -> anyhow::Result<serde_json::Value> {
    let mut stmt = conn.prepare(
        "SELECT
            name,
            display_name,
            type,
            owner_name,
            root_path,
            uplugin_path,
            uproject_path,
            engine_association
         FROM components",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(serde_json::json!({
            "name": row.get::<_, String>(0)?,
            "display_name": row.get::<_, Option<String>>(1)?,
            "type": row.get::<_, Option<String>>(2)?,
            "owner_name": row.get::<_, Option<String>>(3)?,
            "root_path": row.get::<_, Option<String>>(4)?,
            "uplugin_path": row.get::<_, Option<String>>(5)?,
            "uproject_path": row.get::<_, Option<String>>(6)?,
            "engine_association": row.get::<_, Option<String>>(7)?,
        }))
    })?;

    let components: Vec<_> = rows.filter_map(Result::ok).collect();
    Ok(serde_json::json!(components))
}

/// Initialize the persistent completion cache.
/// 初始化持久化补全缓存。
pub fn init_cache_db(conn: &Connection) -> rusqlite::Result<()> {
    create_tables(conn)?;
    create_indices(conn)?;

    let stored_version = conn
        .query_row(
            "SELECT value FROM cache_meta WHERE key = 'completion_cache_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse::<i32>().ok());

    if stored_version != Some(COMPLETION_CACHE_VERSION) {
        conn.execute("DELETE FROM persistent_cache", [])?;
        conn.execute(
            "INSERT OR REPLACE INTO cache_meta (key, value)
             VALUES ('completion_cache_version', ?1)",
            [COMPLETION_CACHE_VERSION.to_string()],
        )?;

        info!(
            "Completion cache version changed ({:?} -> {}), cache cleared.",
            stored_version, COMPLETION_CACHE_VERSION
        );
    }

    Ok(())
}

fn is_header_extension(extension: &str) -> bool {
    matches!(extension, "h" | "hpp" | "hh" | "inl")
}

fn normalize_path_for_compare(path: &str) -> String {
    path.replace('\\', "/").to_ascii_lowercase()
}
