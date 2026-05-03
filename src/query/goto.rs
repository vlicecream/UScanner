use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use tree_sitter::{Node, Parser, Point};

use crate::db::project_path::PATH_CTE;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

const HEADER_PRIORITY_SQL: &str = "
    CASE
        WHEN sf.text LIKE '%.h' THEN 0
        WHEN sf.text LIKE '%.hpp' THEN 1
        WHEN sf.text LIKE '%.inl' THEN 2
        ELSE 3
    END
";

const GENERATED_PRIORITY_SQL: &str = "
    CASE
        WHEN sf.text LIKE '%.generated.h' THEN 1
        ELSE 0
    END
";

// -----------------------------------------------------------------------------
// Public data types
// -----------------------------------------------------------------------------

/// Cursor context extracted from the current buffer.
/// 从当前 buffer 光标位置提取出来的上下文。
#[derive(Debug, Clone)]
pub struct CursorCtx {
    /// Symbol under cursor, such as InitInfo, Title, UTextBlock.
    /// 光标下的符号，比如 InitInfo、Title、UTextBlock。
    pub symbol: String,

    /// Text before ::, ., or ->.
    /// ::、.、-> 前面的文本。
    pub qualifier: Option<String>,

    /// Qualifier operator: ::, ., or ->.
    /// 修饰符操作符：::、.、->。
    pub qualifier_op: Option<String>,

    /// Enclosing class or struct name.
    /// 当前光标所在的类或结构体名称。
    pub enclosing_class: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalDeclMatch {
    row: usize,
    col: usize,
    type_name: Option<String>,
}

// -----------------------------------------------------------------------------
// Basic tree-sitter helpers
// -----------------------------------------------------------------------------

/// Get node text safely.
/// 安全获取 node 对应的源码文本。
fn node_text<'a>(node: &Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// Iterate children without exposing tree-sitter cursor lifetime details.
/// 遍历子节点，隐藏 tree-sitter cursor 生命周期细节。
fn children_of<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

fn is_function_like(kind: &str) -> bool {
    matches!(
        kind,
        "function_definition" | "unreal_function_definition" | "lambda_expression"
    )
}

/// Recursively find the first descendant with the given kind.
/// 递归查找第一个指定 kind 的子孙节点。
fn find_descendant_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }

    for child in children_of(node) {
        if let Some(found) = find_descendant_of_kind(child, kind) {
            return Some(found);
        }
    }

    None
}

/// Return true if this node can represent a useful symbol.
/// 判断这个 node 是否可能是一个有效 symbol。
fn is_symbol_node(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "namespace_identifier"
            | "qualified_identifier"
            | "template_type"
            | "template_function"
            | "template_method"
    )
}

/// Climb from a raw cursor node to a meaningful symbol node.
/// 从光标命中的原始节点向上找到真正有意义的 symbol 节点。
fn normalize_symbol_node<'a>(mut node: Node<'a>) -> Option<Node<'a>> {
    if is_symbol_node(node.kind()) {
        return Some(node);
    }

    while let Some(parent) = node.parent() {
        if is_symbol_node(parent.kind()) {
            return Some(parent);
        }

        if matches!(
            parent.kind(),
            "call_expression"
                | "field_expression"
                | "function_declarator"
                | "declaration"
                | "function_definition"
                | "parameter_declaration"
        ) {
            break;
        }

        node = parent;
    }

    None
}

/// Extract the most useful symbol text from a symbol node.
/// 从 symbol node 中提取最有用的符号文本。
fn symbol_text(node: Node, src: &[u8]) -> String {
    match node.kind() {
        "qualified_identifier" => {
            if let Some(name) = node.child_by_field_name("name") {
                return node_text(&name, src).trim().to_string();
            }
        }
        "template_type" | "template_function" | "template_method" => {
            if let Some(name) = node.child_by_field_name("name") {
                return node_text(&name, src).trim().to_string();
            }
        }
        _ => {}
    }

    node_text(&node, src).trim().to_string()
}

// -----------------------------------------------------------------------------
// Enclosing class helpers
// -----------------------------------------------------------------------------

/// Get the enclosing class or struct for a cursor node.
/// 获取光标所在的类或结构体。
fn get_enclosing_class(node: Node, src: &[u8]) -> Option<String> {
    let mut cur = Some(node);

    while let Some(n) = cur {
        match n.kind() {
            "class_specifier"
            | "struct_specifier"
            | "unreal_class_declaration"
            | "unreal_struct_declaration"
            | "unreal_reflected_class_declaration"
            | "unreal_reflected_struct_declaration" => {
                if let Some(name_node) = n.child_by_field_name("name") {
                    let name = node_text(&name_node, src).trim();
                    if !name.is_empty() {
                        return Some(strip_namespace(name));
                    }
                }
            }

            "function_definition" => {
                if let Some(decl) = n.child_by_field_name("declarator") {
                    if let Some(qi) = find_descendant_of_kind(decl, "qualified_identifier") {
                        if let Some(scope) = qi.child_by_field_name("scope") {
                            let scope_text = node_text(&scope, src).trim();
                            if !scope_text.is_empty() {
                                return Some(strip_namespace(scope_text));
                            }
                        }
                    }
                }
            }

            _ => {}
        }

        cur = n.parent();
    }

    None
}

/// Remove namespace prefix from a type name.
/// 去掉类型名里的 namespace 前缀。
fn strip_namespace(name: &str) -> String {
    name.rsplit("::").next().unwrap_or(name).trim().to_string()
}

// -----------------------------------------------------------------------------
// Cursor context extraction
// -----------------------------------------------------------------------------

/// Extract symbol, qualifier, operator, and enclosing class from cursor position.
/// 从光标位置提取 symbol、修饰对象、操作符和所在类。
pub fn extract_cursor_context(content: &str, line: u32, character: u32) -> Option<CursorCtx> {
    let language: tree_sitter::Language = tree_sitter_unreal_cpp::LANGUAGE.into();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;

    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let src = content.as_bytes();

    let row = line as usize;
    let col = character as usize;
    let raw_node = cursor_node_at(root, row, col)?;
    let node = normalize_symbol_node(raw_node)?;
    let symbol = symbol_text(node, src);

    if symbol.is_empty() || node.is_extra() {
        return None;
    }

    let enclosing_class = get_enclosing_class(node, src);
    let (qualifier, qualifier_op) = extract_qualifier(node, src);

    Some(CursorCtx {
        symbol,
        qualifier,
        qualifier_op,
        enclosing_class,
    })
}

/// Find the node under Neovim's 0-based cursor column.
/// 根据 Neovim 传来的 0-based 光标列查找当前节点。
fn cursor_node_at(root: Node, row: usize, col: usize) -> Option<Node> {
    let current = Point::new(row, col);
    let next = Point::new(row, col.saturating_add(1));

    // Prefer the character under the cursor. This matters at the first
    // character of a word: [col - 1, col] includes the separator before it.
    // 优先取光标下的字符；如果在单词第一个字符，[col - 1, col] 会包含前面的分隔符。
    root.descendant_for_point_range(current, next)
        .or_else(|| {
            let previous = Point::new(row, col.saturating_sub(1));
            root.descendant_for_point_range(previous, current)
        })
        .or_else(|| root.descendant_for_point_range(current, current))
}

fn enclosing_function<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut current = Some(node);

    while let Some(node) = current {
        if is_function_like(node.kind()) {
            return Some(node);
        }

        current = node.parent();
    }

    None
}

fn find_enclosing_function_for_row<'a>(node: Node<'a>, row: usize) -> Option<Node<'a>> {
    if node.start_position().row > row || node.end_position().row < row {
        return None;
    }

    for child in children_of(node) {
        if let Some(found) = find_enclosing_function_for_row(child, row) {
            return Some(found);
        }
    }

    if is_function_like(node.kind()) {
        return Some(node);
    }

    None
}

/// Extract qualifier from expressions like A::B, Obj.Field, Ptr->Field.
/// 从 A::B、Obj.Field、Ptr->Field 这类表达式中提取 qualifier。
fn extract_qualifier(node: Node, src: &[u8]) -> (Option<String>, Option<String>) {
    let mut cur = node.parent();

    while let Some(n) = cur {
        match n.kind() {
            "qualified_identifier" => {
                if let Some(scope) = n.child_by_field_name("scope") {
                    let text = node_text(&scope, src).trim();
                    if !text.is_empty() {
                        return (Some(strip_namespace(text)), Some("::".to_string()));
                    }
                }
                break;
            }

            "field_expression" => {
                let children = children_of(n);

                for (index, child) in children.iter().enumerate() {
                    let op = child.kind();

                    if op == "." || op == "->" {
                        if index > 0 {
                            let object_text = node_text(&children[index - 1], src).trim();
                            if !object_text.is_empty() {
                                return (Some(object_text.to_string()), Some(op.to_string()));
                            }
                        }
                    }
                }

                break;
            }

            _ => {}
        }

        cur = n.parent();
    }

    (None, None)
}

// -----------------------------------------------------------------------------
// Type inference from current buffer
// -----------------------------------------------------------------------------

/// Infer a variable type from declarations in the current buffer.
/// 从当前 buffer 的声明里推断变量类型。
pub fn infer_var_type(content: &str, var_name: &str, cursor_line: Option<u32>) -> Option<String> {
    let language: tree_sitter::Language = tree_sitter_unreal_cpp::LANGUAGE.into();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;

    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let src = content.as_bytes();

    let mut matches = Vec::new();
    if let Some(line) = cursor_line {
        let cursor_row = line as usize;

        if let Some(function) = find_enclosing_function_for_row(root, cursor_row) {
            scan_for_var_decl(function, src, var_name, &mut matches, true);
        }
    }

    if matches.is_empty() {
        scan_for_var_decl(root, src, var_name, &mut matches, false);
    }

    if matches.is_empty() {
        return None;
    }

    if let Some(line) = cursor_line {
        let cursor_row = line as usize;

        if let Some((row, col, ty)) = select_nearest_type_match(&matches, cursor_row) {
            if matches.len() > 1 {
                tracing::info!(
                    "infer_var_type: var='{}' selected='{}' at {}:{} from {} candidates",
                    var_name,
                    ty,
                    row + 1,
                    col,
                    matches.len()
                );
            }
            return Some(ty.clone());
        }
    }

    matches
        .into_iter()
        .min_by_key(|(row, col, _)| (*row, *col))
        .map(|(_, _, ty)| ty)
}

/// Scan declarations and collect possible variable types.
/// 扫描声明节点，收集变量可能的类型。
fn scan_for_var_decl(
    node: Node,
    src: &[u8],
    var_name: &str,
    matches: &mut Vec<(usize, usize, String)>,
    stop_at_nested_functions: bool,
) {
    match node.kind() {
        "declaration" | "parameter_declaration" | "field_declaration" => {
            if let Some(type_node) = node.child_by_field_name("type") {
                if let Some(decl_node) = node.child_by_field_name("declarator") {
                    if let Some(name_node) = extract_decl_name_node(decl_node) {
                        let name = node_text(&name_node, src).trim();
                        if name == var_name {
                            let raw_type = node_text(&type_node, src).trim();
                            let cleaned = clean_type(raw_type);
                            if !cleaned.is_empty() {
                                matches.push((
                                    name_node.start_position().row,
                                    name_node.start_position().column,
                                    cleaned,
                                ));
                            }
                        }
                    }
                }
            }
        }

        _ => {}
    }

    for child in children_of(node) {
        if stop_at_nested_functions && is_function_like(child.kind()) {
            continue;
        }
        scan_for_var_decl(child, src, var_name, matches, stop_at_nested_functions);
    }
}

fn select_nearest_type_match<'a>(
    matches: &'a [(usize, usize, String)],
    cursor_row: usize,
) -> Option<&'a (usize, usize, String)> {
    matches
        .iter()
        .filter(|(row, _, _)| *row <= cursor_row)
        .max_by_key(|(row, col, _)| (*row, *col))
}

fn extract_decl_name_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    match node.kind() {
        "identifier" | "field_identifier" => Some(node),

        "pointer_declarator"
        | "reference_declarator"
        | "array_declarator"
        | "function_declarator"
        | "init_declarator" => {
            if let Some(decl) = node.child_by_field_name("declarator") {
                return extract_decl_name_node(decl);
            }

            for child in children_of(node) {
                if let Some(name) = extract_decl_name_node(child) {
                    return Some(name);
                }
            }

            None
        }

        _ => {
            for child in children_of(node) {
                if let Some(name) = extract_decl_name_node(child) {
                    return Some(name);
                }
            }

            None
        }
    }
}

/// Clean Unreal/C++ type wrappers into a lookup-friendly type name.
/// 把 Unreal/C++ 类型包装清理成适合查库的类型名。
fn clean_type(raw: &str) -> String {
    let mut text = raw
        .replace("const", " ")
        .replace("volatile", " ")
        .replace("class ", " ")
        .replace("struct ", " ")
        .replace('*', " ")
        .replace('&', " ");

    text = text.split_whitespace().collect::<Vec<_>>().join(" ");

    let wrapper_inner = extract_known_unreal_wrapper_inner(&text);
    if let Some(inner) = wrapper_inner {
        return clean_type(&inner);
    }

    strip_namespace(text.trim())
}

/// Extract inner type from common Unreal wrappers.
/// 从常见 Unreal 包装类型中提取内部类型。
fn extract_known_unreal_wrapper_inner(text: &str) -> Option<String> {
    let wrappers = [
        "TObjectPtr",
        "TWeakObjectPtr",
        "TSoftObjectPtr",
        "TSubclassOf",
        "TScriptInterface",
        "TOptional",
        "TSharedPtr",
        "TSharedRef",
        "TUniquePtr",
    ];

    for wrapper in wrappers {
        let prefix = format!("{}<", wrapper);
        if text.starts_with(&prefix) && text.ends_with('>') {
            return Some(text[prefix.len()..text.len() - 1].trim().to_string());
        }
    }

    None
}

// -----------------------------------------------------------------------------
// DB lookup context
// -----------------------------------------------------------------------------

struct GotoCtx<'a> {
    conn: &'a Connection,
    class_id_cache: HashMap<String, Vec<i64>>,
    parent_cache: HashMap<i64, Vec<i64>>,
}

impl<'a> GotoCtx<'a> {
    fn new(conn: &'a Connection) -> Self {
        Self {
            conn,
            class_id_cache: HashMap::new(),
            parent_cache: HashMap::new(),
        }
    }

    /// Get class ids by class name, preferring headers.
    /// 根据类名获取 classes.id，优先返回头文件里的定义。
    fn get_class_ids(&mut self, name: &str) -> Result<Vec<i64>> {
        let name = strip_namespace(name);

        if name.is_empty() {
            return Ok(Vec::new());
        }

        if let Some(ids) = self.class_id_cache.get(&name) {
            return Ok(ids.clone());
        }

        let sql = format!(
            r#"
            SELECT c.id
            FROM classes c
            JOIN strings s ON c.name_id = s.id
            JOIN files f ON c.file_id = f.id
            JOIN strings sf ON f.filename_id = sf.id
            WHERE s.text = ?
            ORDER BY
                {generated_priority},
                {header_priority},
                c.line_number
            "#,
            generated_priority = GENERATED_PRIORITY_SQL,
            header_priority = HEADER_PRIORITY_SQL
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let ids = stmt
            .query_map([name.as_str()], |row| row.get::<_, i64>(0))?
            .filter_map(|row| row.ok())
            .collect::<Vec<_>>();

        self.class_id_cache.insert(name, ids.clone());
        Ok(ids)
    }

    /// Get parent class ids for BFS inheritance traversal.
    /// 获取父类 id，用于 BFS 遍历继承链。
    fn get_parent_ids(&mut self, class_id: i64) -> Result<Vec<i64>> {
        if let Some(ids) = self.parent_cache.get(&class_id) {
            return Ok(ids.clone());
        }

        let mut stmt = self.conn.prepare(
            r#"
            SELECT i.parent_class_id, sp.text
            FROM inheritance i
            JOIN strings sp ON i.parent_name_id = sp.id
            WHERE i.child_id = ?
            "#,
        )?;

        let rows = stmt.query_map([class_id], |row| {
            Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut result = Vec::new();

        for row in rows.filter_map(|row| row.ok()) {
            let (maybe_parent_id, parent_name) = row;

            if let Some(parent_id) = maybe_parent_id {
                result.push(parent_id);
                continue;
            }

            for id in self.get_class_ids(&parent_name)? {
                result.push(id);
            }
        }

        result.sort_unstable();
        result.dedup();

        self.parent_cache.insert(class_id, result.clone());
        Ok(result)
    }
}

// -----------------------------------------------------------------------------
// DB query helpers
// -----------------------------------------------------------------------------

/// Find a member in a class, optionally preferring implementation files.
/// 在某个类里找成员，可优先返回实现文件。
fn find_member_in_class(
    conn: &Connection,
    class_id: i64,
    symbol_name: &str,
    prefer_impl: bool,
) -> Result<Option<Value>> {
    let order_by = member_order_by_clause(prefer_impl);
    let sql = format!(
        r#"
        {}
        SELECT sm.text,
               m.line_number,
               dp.full_path || '/' || sf.text,
               sc.text
        FROM members m
        JOIN strings sm ON m.name_id = sm.id
        JOIN classes c ON m.class_id = c.id
        JOIN strings sc ON c.name_id = sc.id
        JOIN files f ON COALESCE(m.file_id, c.file_id) = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sf ON f.filename_id = sf.id
        WHERE m.class_id = ?
          AND sm.text = ?
        {}
        LIMIT 1
        "#,
        PATH_CTE,
        order_by,
    );

    let mut result = conn.query_row(&sql, params![class_id, symbol_name], |row| {
        Ok(json!({
            "symbol_name": row.get::<_, String>(0)?,
            "line_number": row.get::<_, i64>(1)?,
            "file_path": normalize_path(&row.get::<_, String>(2)?),
            "class_name": row.get::<_, String>(3)?,
        }))
    })
    .optional()?;

    if let Some(value) = result.as_mut() {
        fix_symbol_location(value, symbol_name);
    }

    Ok(result)
}

/// Walk inheritance chain with BFS and find a member definition.
/// 用 BFS 遍历继承链，并查找成员定义。
pub fn find_symbol_in_inheritance_chain(
    conn: &Connection,
    class_name: &str,
    symbol_name: &str,
) -> Result<Option<Value>> {
    find_symbol_in_inheritance_chain_inner(conn, class_name, symbol_name, false)
}

/// Same as find_symbol_in_inheritance_chain but with configurable direction.
/// 同上，但可配置跳转方向。
fn find_symbol_in_inheritance_chain_inner(
    conn: &Connection,
    class_name: &str,
    symbol_name: &str,
    prefer_impl: bool,
) -> Result<Option<Value>> {
    let mut ctx = GotoCtx::new(conn);
    let start_ids = ctx.get_class_ids(class_name)?;

    if start_ids.is_empty() {
        return Ok(None);
    }

    let mut queue = VecDeque::from(start_ids);
    let mut visited = HashSet::new();

    while let Some(class_id) = queue.pop_front() {
        if !visited.insert(class_id) {
            continue;
        }

        if let Some(result) = find_member_in_class(conn, class_id, symbol_name, prefer_impl)? {
            return Ok(Some(result));
        }

        for parent_id in ctx.get_parent_ids(class_id)? {
            if !visited.contains(&parent_id) {
                queue.push_back(parent_id);
            }
        }
    }

    Ok(None)
}

/// Find a class, struct, or enum definition.
/// 查找 class、struct 或 enum 的定义位置。
fn find_type_definition(conn: &Connection, name: &str) -> Result<Option<Value>> {
    let name = strip_namespace(name);

    if name.is_empty() {
        return Ok(None);
    }

    let sql = format!(
        r#"
        {}
        SELECT sc.text,
               c.line_number,
               dp.full_path || '/' || sf.text,
               c.symbol_type
        FROM classes c
        JOIN strings sc ON c.name_id = sc.id
        JOIN files f ON c.file_id = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sf ON f.filename_id = sf.id
        WHERE sc.text = ?
        ORDER BY
            {generated_priority},
            {header_priority},
            c.line_number
        LIMIT 1
        "#,
        PATH_CTE,
        generated_priority = GENERATED_PRIORITY_SQL,
        header_priority = HEADER_PRIORITY_SQL
    );

    let mut result = conn
        .query_row(&sql, [name.as_str()], |row| {
            let symbol_name = row.get::<_, String>(0)?;

            Ok(json!({
                "symbol_name": symbol_name.clone(),
                "line_number": row.get::<_, i64>(1)?,
                "file_path": normalize_path(&row.get::<_, String>(2)?),
                "class_name": symbol_name,
                "kind": row.get::<_, String>(3)?,
            }))
        })
        .optional()?;

    if let Some(value) = result.as_mut() {
        fix_type_definition_location(conn, value, &name)?;
    }

    Ok(result)
}

/// Find a symbol in a specific Unreal module.
/// 在指定 Unreal 模块里查找 symbol。
pub fn find_symbol_in_module(
    conn: &Connection,
    module_name: &str,
    symbol_name: &str,
) -> Result<Option<Value>> {
    if let Some(result) = find_type_in_module(conn, module_name, symbol_name)? {
        return Ok(Some(result));
    }

    find_member_in_module(conn, module_name, symbol_name, false)
}

/// Find a type definition inside a module.
/// 在模块里查找类型定义。
fn find_type_in_module(
    conn: &Connection,
    module_name: &str,
    symbol_name: &str,
) -> Result<Option<Value>> {
    let sql = format!(
        r#"
        {}
        SELECT sc.text,
               c.line_number,
               dp.full_path || '/' || sf.text,
               c.symbol_type
        FROM classes c
        JOIN strings sc ON c.name_id = sc.id
        JOIN files f ON c.file_id = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sf ON f.filename_id = sf.id
        JOIN modules m ON f.module_id = m.id
        JOIN strings sm ON m.name_id = sm.id
        WHERE sm.text = ?
          AND sc.text = ?
        ORDER BY
            {generated_priority},
            {header_priority},
            c.line_number
        LIMIT 1
        "#,
        PATH_CTE,
        generated_priority = GENERATED_PRIORITY_SQL,
        header_priority = HEADER_PRIORITY_SQL
    );

    let mut result = conn
        .query_row(&sql, params![module_name, symbol_name], |row| {
            Ok(json!({
                "symbol_name": row.get::<_, String>(0)?,
                "line_number": row.get::<_, i64>(1)?,
                "file_path": normalize_path(&row.get::<_, String>(2)?),
                "kind": row.get::<_, String>(3)?,
            }))
        })
        .optional()?;

    if let Some(value) = result.as_mut() {
        fix_type_definition_location(conn, value, symbol_name)?;
    }

    Ok(result)
}

/// Find a member inside a module.
/// 在模块里查找成员。
fn find_member_in_module(
    conn: &Connection,
    module_name: &str,
    symbol_name: &str,
    prefer_impl: bool,
) -> Result<Option<Value>> {
    let order_by = member_order_by_clause(prefer_impl);
    let sql = format!(
        r#"
        {}
        SELECT smem.text,
               mem.line_number,
               dp.full_path || '/' || sf.text,
               sc.text
        FROM members mem
        JOIN strings smem ON mem.name_id = smem.id
        JOIN classes c ON mem.class_id = c.id
        JOIN strings sc ON c.name_id = sc.id
        JOIN files f ON COALESCE(mem.file_id, c.file_id) = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sf ON f.filename_id = sf.id
        JOIN modules m ON f.module_id = m.id
        JOIN strings smod ON m.name_id = smod.id
        WHERE smod.text = ?
          AND smem.text = ?
        {}
        LIMIT 1
        "#,
        PATH_CTE,
        order_by,
    );

    let mut result = conn
        .query_row(&sql, params![module_name, symbol_name], |row| {
            Ok(json!({
                "symbol_name": row.get::<_, String>(0)?,
                "line_number": row.get::<_, i64>(1)?,
                "file_path": normalize_path(&row.get::<_, String>(2)?),
                "class_name": row.get::<_, String>(3)?,
            }))
        })
        .optional()?;

    if let Some(value) = result.as_mut() {
        fix_symbol_location(value, symbol_name);
    }

    Ok(result)
}

/// Final fallback: find a member by name anywhere.
/// 最终兜底：在全工程按成员名查找。
fn find_member_anywhere(conn: &Connection, symbol_name: &str, prefer_impl: bool) -> Result<Option<Value>> {
    let order_by = member_order_by_clause(prefer_impl);
    let sql = format!(
        r#"
        {}
        SELECT sm.text,
               m.line_number,
               dp.full_path || '/' || sf.text,
               sc.text
        FROM members m
        JOIN strings sm ON m.name_id = sm.id
        JOIN classes c ON m.class_id = c.id
        JOIN strings sc ON c.name_id = sc.id
        JOIN files f ON COALESCE(m.file_id, c.file_id) = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sf ON f.filename_id = sf.id
        WHERE sm.text = ?
        {}
        LIMIT 1
        "#,
        PATH_CTE,
        order_by,
    );

    let mut result = conn
        .query_row(&sql, [symbol_name], |row| {
            Ok(json!({
                "symbol_name": row.get::<_, String>(0)?,
                "line_number": row.get::<_, i64>(1)?,
                "file_path": normalize_path(&row.get::<_, String>(2)?),
                "class_name": row.get::<_, String>(3)?,
            }))
        })
        .optional()?;

    if let Some(value) = result.as_mut() {
        fix_symbol_location(value, symbol_name);
    }

    Ok(result)
}

// -----------------------------------------------------------------------------
// ORDER BY helpers
// -----------------------------------------------------------------------------

/// Build ORDER BY clause for member queries based on direction.
/// 根据跳转方向构造成员的 ORDER BY 子句。
fn member_order_by_clause(prefer_impl: bool) -> String {
    if prefer_impl {
        r#"
    ORDER BY
        CASE WHEN m.access = 'impl' THEN 0 ELSE 1 END,
        CASE
            WHEN sf.text LIKE '%.cpp' THEN 0
            WHEN sf.text LIKE '%.cc' THEN 1
            WHEN sf.text LIKE '%.cxx' THEN 2
            ELSE 3
        END,
        m.line_number
    "#
        .to_string()
    } else {
        format!(
            r#"
    ORDER BY
        CASE WHEN m.access = 'impl' THEN 1 ELSE 0 END,
        {},
        {},
        m.line_number
    "#,
            GENERATED_PRIORITY_SQL.trim(),
            HEADER_PRIORITY_SQL.trim(),
        )
    }
}

// -----------------------------------------------------------------------------
// Implementation lookup by class name
// -----------------------------------------------------------------------------

/// Resolve the class name to search for in implementation mode.
/// 在实现模式下，解析要查找的类名。
fn resolve_impl_class(ctx: &CursorCtx, content: &str, cursor_line: u32) -> Option<String> {
    if let Some(ref qualifier) = ctx.qualifier {
        if ctx.qualifier_op.as_deref() == Some("::") {
            if qualifier == "Super" {
                return ctx.enclosing_class.clone();
            }
            return Some(qualifier.clone());
        }
        if matches!(ctx.qualifier_op.as_deref(), Some(".") | Some("->")) {
            if qualifier == "this" {
                return ctx.enclosing_class.clone();
            }
            return infer_var_type(content, qualifier, Some(cursor_line));
        }
    }
    ctx.enclosing_class.clone()
}

fn find_local_declaration(
    content: &str,
    symbol_name: &str,
    line: u32,
    character: u32,
) -> Option<LocalDeclMatch> {
    let language: tree_sitter::Language = tree_sitter_unreal_cpp::LANGUAGE.into();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;

    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let src = content.as_bytes();
    let row = line as usize;
    let col = character as usize;
    let raw_node = cursor_node_at(root, row, col)?;
    let function = enclosing_function(raw_node)?;
    let mut matches = Vec::new();
    scan_local_declarations(function, src, symbol_name, &mut matches, true);

    matches
        .into_iter()
        .filter(|item| item.row < row || (item.row == row && item.col <= col))
        .max_by_key(|item| (item.row, item.col))
}

fn scan_local_declarations(
    node: Node,
    src: &[u8],
    symbol_name: &str,
    matches: &mut Vec<LocalDeclMatch>,
    is_root: bool,
) {
    if !is_root && is_function_like(node.kind()) {
        return;
    }

    if matches!(node.kind(), "declaration" | "parameter_declaration") {
        if let Some(decl_node) = node.child_by_field_name("declarator") {
            if let Some(name_node) = extract_decl_name_node(decl_node) {
                let name = node_text(&name_node, src).trim();
                if name == symbol_name {
                    let type_name = node
                        .child_by_field_name("type")
                        .map(|type_node| clean_type(node_text(&type_node, src).trim()));

                    matches.push(LocalDeclMatch {
                        row: name_node.start_position().row,
                        col: name_node.start_position().column,
                        type_name,
                    });
                }
            }
        }
    }

    for child in children_of(node) {
        scan_local_declarations(child, src, symbol_name, matches, false);
    }
}

/// Find a member by class name, not class_id. Hits both .h and .cpp records.
/// 按类名查找成员（非 class_id），同时命中 .h 和 .cpp 记录。
fn find_member_by_class_name(
    conn: &Connection,
    class_name: &str,
    symbol_name: &str,
    prefer_impl: bool,
) -> Result<Option<Value>> {
    let name = strip_namespace(class_name);
    if name.is_empty() {
        return Ok(None);
    }

    let order_by = member_order_by_clause(prefer_impl);
    let sql = format!(
        r#"
        {}
        SELECT sm.text, m.line_number, dp.full_path || '/' || sf.text, sc.text
        FROM members m
        JOIN strings sm ON m.name_id = sm.id
        JOIN classes c ON m.class_id = c.id
        JOIN strings sc ON c.name_id = sc.id
        JOIN files f ON COALESCE(m.file_id, c.file_id) = f.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sf ON f.filename_id = sf.id
        WHERE c.name_id IN (SELECT id FROM strings WHERE text = ?)
          AND sm.text = ?
        {}
        LIMIT 1
        "#,
        PATH_CTE,
        order_by,
    );
    // strip_namespace already handles the name, but let me double-check
    let key_name = name.clone();
    let mut result = conn
        .query_row(&sql, params![key_name, symbol_name], |row| {
            Ok(json!({
                "symbol_name": row.get::<_, String>(0)?,
                "line_number": row.get::<_, i64>(1)?,
                "file_path": normalize_path(&row.get::<_, String>(2)?),
                "class_name": row.get::<_, String>(3)?,
            }))
        })
        .optional()?;

    if let Some(value) = result.as_mut() {
        fix_symbol_location(value, symbol_name);
    }

    Ok(result)
}

/// Get class name from class_id.
fn get_class_name_by_id(conn: &Connection, class_id: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT s.text FROM classes c JOIN strings s ON c.name_id = s.id WHERE c.id = ?",
        [class_id],
        |row| row.get(0),
    )?)
}

/// Walk inheritance chain looking for implementation by class name.
/// 遍历继承链，按类名查找实现。
fn find_impl_in_inheritance(
    conn: &Connection,
    class_name: &str,
    symbol_name: &str,
) -> Result<Option<Value>> {
    let name = strip_namespace(class_name);
    if name.is_empty() {
        return Ok(None);
    }

    if let Some(result) = find_member_by_class_name(conn, &name, symbol_name, true)? {
        return Ok(Some(result));
    }

    let mut gctx = GotoCtx::new(conn);
    let start_ids = gctx.get_class_ids(&name)?;
    let mut queue = VecDeque::from(start_ids);
    let mut visited = HashSet::new();
    let mut tried_names = HashSet::new();
    tried_names.insert(name.to_string());

    while let Some(class_id) = queue.pop_front() {
        if !visited.insert(class_id) {
            continue;
        }

        for parent_id in gctx.get_parent_ids(class_id)? {
            if let Ok(parent_name) = get_class_name_by_id(conn, parent_id) {
                let parent_short = strip_namespace(&parent_name);
                if !parent_short.is_empty() && tried_names.insert(parent_short.clone()) {
                    if let Some(result) =
                        find_member_by_class_name(conn, &parent_short, symbol_name, true)?
                    {
                        return Ok(Some(result));
                    }
                }
            }

            if !visited.contains(&parent_id) {
                queue.push_back(parent_id);
            }
        }
    }

    Ok(None)
}

// -----------------------------------------------------------------------------
// Main entry
// -----------------------------------------------------------------------------

/// Main Go to Definition entry point (prefers header declarations).
/// Go to Definition 的主入口（优先头文件声明）。
pub fn goto_definition(
    conn: &Connection,
    content: String,
    line: u32,
    character: u32,
    file_path: Option<String>,
) -> Result<Value> {
    goto_definition_inner(conn, content, line, character, file_path, false)
}

/// Go to Implementation entry point (prefers .cpp definitions).
/// Go to Implementation 主入口（优先 .cpp 实现）。
pub fn goto_implementation(
    conn: &Connection,
    content: String,
    line: u32,
    character: u32,
    file_path: Option<String>,
) -> Result<Value> {
    goto_definition_inner(conn, content, line, character, file_path, true)
}

fn goto_definition_inner(
    conn: &Connection,
    content: String,
    line: u32,
    character: u32,
    file_path: Option<String>,
    prefer_impl: bool,
) -> Result<Value> {
    let Some(ctx) = extract_cursor_context(&content, line, character) else {
        return Ok(Value::Null);
    };

    let mode = if prefer_impl { "implementation" } else { "definition" };
    tracing::debug!(
        "goto_{}: symbol='{}', qualifier={:?}, op={:?}, enclosing={:?}, line={}, character={}",
        mode,
        ctx.symbol,
        ctx.qualifier,
        ctx.qualifier_op,
        ctx.enclosing_class,
        line + 1,
        character
    );

    if let Some(local_decl) = find_local_declaration(&content, &ctx.symbol, line, character) {
        if let Some(ref path) = file_path {
            tracing::debug!(
                "goto_{}: resolved local symbol '{}' to {}:{} type={:?}",
                mode,
                ctx.symbol,
                local_decl.row + 1,
                local_decl.col,
                local_decl.type_name
            );

            return Ok(json!({
                "symbol_name": ctx.symbol,
                "line_number": (local_decl.row + 1) as i64,
                "col": local_decl.col as i64,
                "file_path": normalize_path(path),
                "source": "local",
            }));
        }
    }

    // Implementation mode: class-name-based search (hits both .h and .cpp
    // records). No global fallback — members from unrelated classes shouldn't
    // be returned as implementations.
    // 实现模式：按类名搜。不全局兜底，避免跳到无关类的同名成员。
    if prefer_impl {
        if let Some(ref name) = resolve_impl_class(&ctx, &content, line) {
            if let Some(result) = find_impl_in_inheritance(conn, name, &ctx.symbol)? {
                tracing::debug!(
                    "goto_{}: resolved '{}' through impl class '{}'",
                    mode,
                    ctx.symbol,
                    name
                );
                return Ok(result);
            }
            if let Some(result) = find_member_by_class_name(conn, name, &ctx.symbol, false)? {
                tracing::debug!(
                    "goto_{}: fell back to class member '{}' on '{}'",
                    mode,
                    ctx.symbol,
                    name
                );
                return Ok(result);
            }
        }
        tracing::debug!("goto_{}: no result for '{}'", mode, ctx.symbol);
        return Ok(Value::Null);
    }

    // 1. If there is an explicit qualifier, resolve through that first.
    // 1. 如果存在显式修饰对象，优先通过它解析。
    if let Some(ref qualifier) = ctx.qualifier {
        let resolved_class = match ctx.qualifier_op.as_deref() {
            Some("::") => {
                if qualifier == "Super" {
                    ctx.enclosing_class.clone().unwrap_or_else(|| qualifier.clone())
                } else {
                    qualifier.clone()
                }
            }

            Some(".") | Some("->") => {
                if qualifier == "this" {
                    ctx.enclosing_class.clone().unwrap_or_else(|| qualifier.clone())
                } else {
                    infer_var_type(&content, qualifier, Some(line))
                        .unwrap_or_else(|| qualifier.clone())
                }
            }

            _ => qualifier.clone(),
        };

        if let Some(result) =
            find_symbol_in_inheritance_chain(conn, &resolved_class, &ctx.symbol)?
        {
            tracing::debug!(
                "goto_{}: resolved '{}' via qualifier class '{}'",
                mode,
                ctx.symbol,
                resolved_class
            );
            return Ok(result);
        }
    }

    // 2. Try member lookup from the enclosing class.
    // 2. 尝试从当前所在类里查成员。
    if let Some(ref enclosing_class) = ctx.enclosing_class {
        if let Some(result) =
            find_symbol_in_inheritance_chain(conn, enclosing_class, &ctx.symbol)?
        {
            tracing::debug!(
                "goto_{}: resolved '{}' via enclosing class '{}'",
                mode,
                ctx.symbol,
                enclosing_class
            );
            return Ok(result);
        }
    }

    // 3. Try type definition lookup.
    // 3. 尝试按类型定义查找。
    if let Some(result) = find_type_definition(conn, &ctx.symbol)? {
        tracing::debug!(
            "goto_{}: resolved '{}' as type definition",
            mode,
            ctx.symbol
        );
        return Ok(result);
    }

    // 4. Final fallback: member search across the whole project.
    // 4. 最终兜底：全工程成员名搜索。
    if let Some(result) = find_member_anywhere(conn, &ctx.symbol, false)? {
        tracing::debug!(
            "goto_{}: resolved '{}' via global member fallback",
            mode,
            ctx.symbol
        );
        return Ok(result);
    }

    tracing::debug!("goto_{}: no result for '{}'", mode, ctx.symbol);
    Ok(Value::Null)
}

// -----------------------------------------------------------------------------
// Misc helpers
// -----------------------------------------------------------------------------

/// Normalize path separators for Neovim/UI.
/// 统一路径分隔符，方便 Neovim/UI 使用。
fn normalize_path(path: &str) -> String {
    path.replace('\\', "/").replace("//", "/")
}

/// Fix DB rows that point at implementation files or miss the exact declaration line.
/// 修正 DB 里指向实现文件，或缺少精确声明行的类型定义结果。
fn fix_type_definition_location(
    conn: &Connection,
    value: &mut Value,
    symbol_name: &str,
) -> Result<()> {
    let line_number = value
        .get("line_number")
        .and_then(Value::as_i64)
        .unwrap_or_default();

    let Some(file_path) = value.get("file_path").and_then(Value::as_str) else {
        return Ok(());
    };

    // Class definitions should prefer headers. DB rows can currently point at helper
    // classes in .cpp files when the same symbol appears in member fields.
    // 类型定义优先跳 header；当前 DB 可能因为 .cpp 里的字段类型误指到实现文件。
    if !is_header_file(file_path) {
        if let Some((header_path, line)) = find_header_type_declaration(conn, symbol_name)? {
            value["file_path"] = json!(header_path);
            value["line_number"] = json!(line as i64);
            fix_symbol_location(value, symbol_name);
            return Ok(());
        }
    }

    if line_number <= 1 {
        if let Some(line) = find_type_declaration_line(file_path, symbol_name) {
            value["line_number"] = json!(line as i64);
        }
    }

    fix_symbol_location(value, symbol_name);

    Ok(())
}

/// Fix a symbol/member row to the exact line and column containing the symbol name.
/// 把符号/成员位置修正到真正包含符号名的行和列。
fn fix_symbol_location(value: &mut Value, symbol_name: &str) {
    let Some(file_path) = value.get("file_path").and_then(Value::as_str) else {
        return;
    };

    let start_line = value
        .get("line_number")
        .and_then(Value::as_i64)
        .unwrap_or(1)
        .max(1) as usize;

    if let Some((line, col)) = find_symbol_location_near(file_path, symbol_name, start_line) {
        value["line_number"] = json!(line as i64);
        value["col"] = json!(col as i64);
    }
}

fn find_symbol_location_near(
    file_path: &str,
    symbol_name: &str,
    start_line: usize,
) -> Option<(usize, usize)> {
    let content = fs::read_to_string(file_path).ok()?;
    let lines = content.lines().collect::<Vec<_>>();

    if lines.is_empty() {
        return None;
    }

    let start = start_line.saturating_sub(1).min(lines.len() - 1);
    let end = (start + 8).min(lines.len() - 1);

    for (index, line) in lines.iter().enumerate().take(end + 1).skip(start) {
        if let Some(col) = find_identifier_in_line(line, symbol_name) {
            return Some((index + 1, col));
        }
    }

    None
}

fn find_identifier_in_line(line: &str, symbol_name: &str) -> Option<usize> {
    let mut start = 0usize;

    while start + symbol_name.len() <= line.len() {
        let Some(relative) = line[start..].find(symbol_name) else {
            return None;
        };

        let absolute = start + relative;
        let before_ok = absolute == 0
            || !is_identifier_byte(line.as_bytes()[absolute.saturating_sub(1)]);
        let end = absolute + symbol_name.len();
        let after_ok = end >= line.len() || !is_identifier_byte(line.as_bytes()[end]);

        if before_ok && after_ok {
            return Some(absolute);
        }

        start = absolute + 1;
    }

    None
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Find a matching header in the indexed files and scan it for the real declaration.
/// 在已索引文件里寻找匹配 header，并扫描真正的类型声明。
fn find_header_type_declaration(
    conn: &Connection,
    symbol_name: &str,
) -> Result<Option<(String, usize)>> {
    let stem = unreal_type_file_stem(symbol_name);
    let exact_h = format!("{stem}.h");
    let exact_hpp = format!("{stem}.hpp");
    let like_h = format!("%{stem}%.h");
    let like_hpp = format!("%{stem}%.hpp");

    let sql = format!(
        r#"
        {}
        SELECT dp.full_path || '/' || sf.text
        FROM files f
        JOIN strings sf ON f.filename_id = sf.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        WHERE sf.text NOT LIKE '%.generated.h'
          AND (
            sf.text = ?
            OR sf.text = ?
            OR sf.text LIKE ?
            OR sf.text LIKE ?
          )
        ORDER BY
          CASE
            WHEN dp.full_path LIKE '%/Classes/%' THEN 0
            WHEN dp.full_path LIKE '%/Public/%' THEN 1
            WHEN dp.full_path LIKE '%/Private/%' THEN 2
            ELSE 3
          END,
          LENGTH(dp.full_path || '/' || sf.text)
        LIMIT 50
        "#,
        PATH_CTE
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![exact_h, exact_hpp, like_h, like_hpp], |row| {
        row.get::<_, String>(0)
    })?;

    for row in rows {
        let path = normalize_path(&row?);
        if let Some(line) = find_type_declaration_line(&path, symbol_name) {
            return Ok(Some((path, line)));
        }
    }

    Ok(None)
}

/// Return true when a path is a C++ header-ish file.
/// 判断路径是否是 C++ 头文件类文件。
fn is_header_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".h") || lower.ends_with(".hpp") || lower.ends_with(".hh")
}

/// Convert an Unreal type name into the most likely file stem.
/// 把 Unreal 类型名转换成最可能的文件名主体。
fn unreal_type_file_stem(symbol_name: &str) -> String {
    strip_unreal_type_prefix(symbol_name).unwrap_or_else(|| symbol_name.to_string())
}

/// Remove common Unreal type prefixes for file-name lookup, e.g. UWidget -> Widget.
/// 为文件名查找去掉常见 Unreal 类型前缀，比如 UWidget -> Widget。
fn strip_unreal_type_prefix(symbol_name: &str) -> Option<String> {
    let mut chars = symbol_name.chars();
    let first = chars.next()?;
    let second = chars.next()?;

    if matches!(first, 'A' | 'U' | 'F' | 'E' | 'T' | 'S') && second.is_ascii_uppercase() {
        Some(symbol_name[first.len_utf8()..].to_string())
    } else {
        None
    }
}

/// Find the real class/struct/enum declaration line inside a source file.
/// 在源码文件里查找真正的 class/struct/enum 声明行。
fn find_type_declaration_line(file_path: &str, symbol_name: &str) -> Option<usize> {
    let content = fs::read_to_string(file_path).ok()?;
    let lines: Vec<&str> = content.lines().collect();

    for index in 0..lines.len() {
        let current = strip_line_comment(lines[index]);

        if !has_type_keyword(&current) {
            continue;
        }

        // Some declarations split the API macro and type name across lines.
        // 有些声明会把 API macro 和类型名拆到多行，所以向后拼几行一起判断。
        let mut window = current;
        for offset in 1..=2 {
            if let Some(next_line) = lines.get(index + offset) {
                window.push(' ');
                window.push_str(&strip_line_comment(next_line));
            }
        }

        if is_type_declaration_text(&window, symbol_name) {
            return Some(index + 1);
        }
    }

    None
}

/// Return true when a line has C++ type declaration keywords.
/// 判断这一行是否包含 C++ 类型声明关键字。
fn has_type_keyword(line: &str) -> bool {
    tokens(line)
        .iter()
        .any(|token| matches!(*token, "class" | "struct" | "enum"))
}

/// Return true when text looks like a definition/declaration for this type.
/// 判断文本是否像目标类型的 class/struct/enum 定义或声明。
fn is_type_declaration_text(text: &str, symbol_name: &str) -> bool {
    let trimmed = text.trim();

    // Skip plain forward declarations like `class AActor;`.
    // 跳过 `class AActor;` 这种纯前置声明。
    if trimmed.ends_with(';') && !trimmed.contains('{') && !trimmed.contains(':') {
        return false;
    }

    let head = declaration_head(trimmed);
    let token_list = tokens(head);

    for (index, token) in token_list.iter().enumerate() {
        if !matches!(*token, "class" | "struct" | "enum") {
            continue;
        }

        if declared_type_name_after_keyword(&token_list, index)
            .is_some_and(|candidate| candidate == symbol_name)
        {
            return true;
        }
    }

    false
}

/// Keep only the declaration head before inheritance/body/forward-decl markers.
/// 只保留继承列表、函数体、前置声明标记之前的声明头。
fn declaration_head(text: &str) -> &str {
    text.find([':', '{', ';'])
        .map_or(text, |boundary| &text[..boundary])
}

/// Extract the declared type name after class/struct/enum.
/// 提取 class/struct/enum 后真正被声明的类型名。
fn declared_type_name_after_keyword<'a>(tokens: &'a [&str], keyword_index: usize) -> Option<&'a str> {
    let keyword = tokens.get(keyword_index)?;
    let mut index = keyword_index + 1;

    if *keyword == "enum" && matches!(tokens.get(index), Some(&"class" | &"struct")) {
        index += 1;
    }

    while let Some(token) = tokens.get(index) {
        if !is_type_declaration_modifier(token) {
            return Some(token);
        }

        index += 1;
    }

    None
}

/// Return true for tokens that can appear between `class` and the real name.
/// 判断哪些 token 可能出现在 `class` 和真实类型名之间。
fn is_type_declaration_modifier(token: &str) -> bool {
    token.ends_with("_API")
        || matches!(
            token,
            "NO_API"
                | "final"
                | "abstract"
                | "alignas"
                | "__declspec"
                | "dllexport"
                | "dllimport"
        )
}

/// Strip single-line comments while keeping declaration text.
/// 去掉单行注释，保留声明本体。
fn strip_line_comment(line: &str) -> String {
    line.split_once("//").map_or(line, |(head, _)| head).to_string()
}

/// Tokenize C++ text into identifier-like tokens.
/// 把 C++ 文本切成近似 identifier 的 token。
fn tokens(text: &str) -> Vec<&str> {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|token| !token.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{find_local_declaration, infer_var_type};

    const SAMPLE: &str = r#"
void StartDeath()
{
    UAlphaType* HealthComponent = GetAlpha();
    HealthComponent->StartDeath();
}

void FinishDeath()
{
    UBetaType* HealthComponent = GetBeta();
    HealthComponent->FinishDeath();
}

void ActivateAbility()
{
    UAbilitySystemComponent* ASC = GetAbilitySystemComponentFromActorInfo();
    ASC->CancelAllAbilities();
}
"#;

    fn line_and_col(content: &str, needle: &str, occurrence: usize) -> (u32, u32) {
        let mut found = 0usize;

        for (row, line) in content.lines().enumerate() {
            let mut offset = 0usize;

            while let Some(col) = line[offset..].find(needle) {
                if found == occurrence {
                    return (row as u32, (offset + col) as u32);
                }

                found += 1;
                offset += col + needle.len();
            }
        }

        panic!("needle not found: {needle} ({occurrence})");
    }

    #[test]
    fn infer_var_type_prefers_nearest_preceding_declaration() {
        let (line, _) = line_and_col(SAMPLE, "HealthComponent->FinishDeath", 0);
        assert_eq!(
            infer_var_type(SAMPLE, "HealthComponent", Some(line)),
            Some("UBetaType".to_string())
        );
    }

    #[test]
    fn find_local_declaration_stays_inside_current_function() {
        let (line, col) = line_and_col(SAMPLE, "HealthComponent->FinishDeath", 0);
        let decl = find_local_declaration(SAMPLE, "HealthComponent", line, col)
            .expect("expected local declaration");

        assert_eq!(decl.row + 1, 10);
    }

    #[test]
    fn find_local_declaration_resolves_simple_local_variable() {
        let (line, col) = line_and_col(SAMPLE, "ASC->CancelAllAbilities", 0);
        let decl = find_local_declaration(SAMPLE, "ASC", line, col)
            .expect("expected local declaration");

        assert_eq!(decl.row + 1, 16);
    }
}
