use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tree_sitter::{Node, Parser, Point};

use crate::server::state::CompletionCache;

const MAX_COMPLETION_ITEMS: usize = 128;
const MAX_MEMBER_ITEMS_PER_CLASS: usize = 96;
const MAX_TYPEDEF_DEPTH: usize = 4;
const MIN_GLOBAL_PREFIX_LEN: usize = 3;
const MIN_ENGINE_GLOBAL_PREFIX_LEN: usize = 4;
const COMPLETION_MATCH_NONE: usize = usize::MAX;
const STRONG_MATCH_SCORE_OFFSET: i64 = 10;
const STRONG_MATCH_TARGET: usize = 24;

/// Per-request cache and lookup context.
/// 单次补全请求里的缓存和查询上下文。
struct CompletionContext<'a> {
    conn: &'a Connection,
    file_cache: HashMap<String, Vec<String>>,
    string_id_cache: HashMap<String, i64>,
    class_id_cache: HashMap<String, Vec<i64>>,
    inheritance_cache: HashMap<(String, String), bool>,
    current_file_id: Option<i64>,
    included_file_ids: Option<HashSet<i64>>,
}

impl<'a> CompletionContext<'a> {
    /// Create a new request context.
    /// 创建新的补全请求上下文。
    fn new(conn: &'a Connection, file_path: Option<&str>) -> Self {
        Self {
            conn,
            file_cache: HashMap::new(),
            string_id_cache: HashMap::new(),
            class_id_cache: HashMap::new(),
            inheritance_cache: HashMap::new(),
            current_file_id: file_path.and_then(|path| get_file_id_by_full_path(conn, path)),
            included_file_ids: None,
        }
    }

    /// Get string id from strings table.
    /// 从 strings 表获取字符串 id。
    fn string_id(&mut self, text: &str) -> Result<Option<i64>> {
        let text = text.trim();

        if text.is_empty() {
            return Ok(None);
        }

        if let Some(id) = self.string_id_cache.get(text) {
            return Ok(Some(*id));
        }

        let id = self
            .conn
            .query_row("SELECT id FROM strings WHERE text = ?", [text], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?;

        if let Some(id) = id {
            self.string_id_cache.insert(text.to_string(), id);
        }

        Ok(id)
    }

    /// Find classes by name, supporting namespace fallback.
    /// 根据类名查 classes.id，支持 namespace 兜底。
    fn class_ids_by_name(&mut self, class_name: &str) -> Result<Vec<i64>> {
        let class_name = clean_type(class_name);

        if class_name.is_empty() {
            return Ok(Vec::new());
        }

        if let Some(ids) = self.class_id_cache.get(&class_name) {
            return Ok(ids.clone());
        }

        let mut ids = Vec::new();

        if let Some(name_id) = self.string_id(&class_name)? {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM classes WHERE name_id = ? ORDER BY line_number",
            )?;

            ids = stmt
                .query_map([name_id], |row| row.get::<_, i64>(0))?
                .filter_map(|row| row.ok())
                .collect();
        }

        if ids.is_empty() && class_name.contains("::") {
            let short = class_name.rsplit("::").next().unwrap_or(&class_name);
            ids = self.class_ids_by_name(short)?;
        }

        ids = self.filter_class_ids_by_includes(ids);

        self.class_id_cache.insert(class_name, ids.clone());
        Ok(ids)
    }

    /// Lazily collect transitive include file ids.
    /// 懒加载当前文件递归 include 到的 file_id 集合。
    fn included_file_ids(&mut self) -> &HashSet<i64> {
        if self.included_file_ids.is_some() {
            return self.included_file_ids.as_ref().unwrap();
        }

        let mut included = HashSet::new();

        if let Some(root_id) = self.current_file_id {
            let mut queue = VecDeque::from([root_id]);

            while let Some(file_id) = queue.pop_front() {
                if !included.insert(file_id) {
                    continue;
                }

                if let Ok(mut stmt) = self.conn.prepare_cached(
                    "SELECT resolved_file_id FROM file_includes WHERE file_id = ? AND resolved_file_id IS NOT NULL",
                ) {
                    if let Ok(rows) = stmt.query_map([file_id], |row| row.get::<_, i64>(0)) {
                        for id in rows.filter_map(|row| row.ok()) {
                            if !included.contains(&id) {
                                queue.push_back(id);
                            }
                        }
                    }
                }
            }
        }

        self.included_file_ids = Some(included);
        self.included_file_ids.as_ref().unwrap()
    }

    /// Prefer class definitions reachable from current file includes.
    /// 同名类很多时，优先使用当前文件 include 链里能看到的定义。
    fn filter_class_ids_by_includes(&mut self, ids: Vec<i64>) -> Vec<i64> {
        if ids.len() <= 1 || self.current_file_id.is_none() {
            return ids;
        }

        let included = self.included_file_ids().clone();

        let filtered = ids
            .iter()
            .copied()
            .filter(|class_id| {
                self.conn
                    .query_row("SELECT file_id FROM classes WHERE id = ?", [class_id], |row| {
                        row.get::<_, Option<i64>>(0)
                    })
                    .ok()
                    .flatten()
                    .map(|file_id| included.contains(&file_id))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();

        if filtered.is_empty() {
            ids
        } else {
            filtered
        }
    }
}

// -----------------------------------------------------------------------------
// Public entry
// -----------------------------------------------------------------------------

/// Main completion entry point.
/// 补全主入口。
pub fn process_completion(
    conn: &Connection,
    content: &str,
    line: u32,
    character: u32,
    file_path: Option<String>,
    cache: Option<Arc<Mutex<CompletionCache>>>,
    persistent_cache: Option<Arc<Mutex<Connection>>>,
) -> Result<Value> {
    process_completion_with_engine(
        conn,
        None,
        content,
        line,
        character,
        file_path,
        cache,
        persistent_cache,
    )
}

/// Main completion entry point with optional Engine DB fallback.
/// 带可选 Engine DB 兜底的补全主入口。
pub fn process_completion_with_engine(
    conn: &Connection,
    engine_conn: Option<&Connection>,
    content: &str,
    line: u32,
    character: u32,
    file_path: Option<String>,
    cache: Option<Arc<Mutex<CompletionCache>>>,
    persistent_cache: Option<Arc<Mutex<Connection>>>,
) -> Result<Value> {
    let mut ctx = CompletionContext::new(conn, file_path.as_deref());
    let mut engine_ctx = engine_conn.map(|conn| CompletionContext::new(conn, file_path.as_deref()));

    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_unreal_cpp::LANGUAGE.into();
    parser.set_language(&language)?;

    let tree = parser
        .parse(content, None)
        .ok_or_else(|| anyhow::anyhow!("failed to parse current buffer"))?;

    let root = tree.root_node();
    let cursor_node = cursor_node(root, line, character)
        .ok_or_else(|| anyhow::anyhow!("no tree-sitter node at cursor"))?;
    let buffer_inheritance = build_buffer_inheritance_map(root, content);

    if let Some(items) = complete_include_paths(&mut ctx, engine_ctx.as_mut(), content, line, character)? {
        return Ok(json!(items));
    }

    if let Some(items) = complete_preprocessor_directives(content, line, character) {
        return Ok(items);
    }

    if let Some(items) = complete_macro_specifiers_at(content, line, character) {
        return Ok(items);
    }

    if let Some(items) = complete_macro_specifiers(cursor_node, content) {
        return Ok(items);
    }

    if let Some(request) = member_completion_request(cursor_node, content) {
        let receiver_text = clean_type(node_text(request.receiver, content));
        let current_class = enclosing_class(cursor_node, content);

        if receiver_text == "Super" {
            if let Some(current_class) = current_class.as_deref() {
                let members = fetch_super_members_with_engine(
                    &mut ctx,
                    engine_ctx.as_mut(),
                    current_class,
                    request.prefix.clone(),
                    &buffer_inheritance,
                    cache,
                    persistent_cache,
                )?;

                let final_items = dedupe_completion_items(members);
                return Ok(json!(final_items));
            }

            return Ok(json!([]));
        }

        let ty = resolve_expression_type_with_engine(
            &mut ctx,
            engine_ctx.as_mut(),
            request.receiver,
            &buffer_inheritance,
            root,
            content,
            line as usize,
        )?;

        if let Some(ty) = ty {
            let ty = resolve_typedef(&mut ctx, &ty)?;

            let members = fetch_members_with_engine(
                &mut ctx,
                engine_ctx.as_mut(),
                &ty,
                request.prefix.clone(),
                &buffer_inheritance,
                cache,
                persistent_cache,
                current_class.as_deref(),
                false,
                false,
            )?;

            let final_items = dedupe_completion_items(members);

            return Ok(json!(final_items));
        }

        return Ok(json!([]));
    }

    let current_class = enclosing_class(cursor_node, content);
    let declaration_context = current_class.is_some()
        && byte_offset_at(content, line as usize, character as usize)
            .map(|offset| is_member_declaration_context_text(&content[..offset]))
            .unwrap_or(false);
    let prefix = text_prefix_at(content, line as usize, character as usize)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| completion_prefix(cursor_node, content));
    let mut items = collect_local_completion_items(
        cursor_node,
        content,
        line as usize,
        character as usize,
        &prefix,
    );

    let buffer_items = collect_buffer_symbol_items(root, content, line as usize, &prefix);
    merge_completion_items(&mut items, buffer_items, MAX_COMPLETION_ITEMS);

    let mut class_member_count = 0usize;
    if !prefix.is_empty() {
        if let Some(current_class) = current_class.as_deref() {
            let mut members = fetch_members_with_engine(
                &mut ctx,
                engine_ctx.as_mut(),
                current_class,
                Some(prefix.clone()),
                &buffer_inheritance,
                cache.clone(),
                persistent_cache.clone(),
                Some(current_class),
                false,
                declaration_context,
            )?;
            if declaration_context {
                apply_declaration_completion_edits(
                    &mut members,
                    content,
                    line,
                    character,
                    &prefix,
                );
            }
            class_member_count = members.len();

            merge_completion_items(&mut items, members, MAX_COMPLETION_ITEMS);
        }
    }

    let cpp_snippet_items = cpp_snippets(&prefix);
    merge_completion_items(&mut items, cpp_snippet_items, MAX_COMPLETION_ITEMS);

    let keyword_items = cpp_keyword_items(&prefix);
    merge_completion_items(&mut items, keyword_items, MAX_COMPLETION_ITEMS);

    if should_offer_ue_snippets(&prefix) {
        let snippets = ue_snippets(&prefix);
        merge_completion_items(&mut items, snippets, MAX_COMPLETION_ITEMS);
    }

    let prefix_len = prefix.chars().count();
    let has_class_context = current_class.is_some();
    let suppress_globals_for_class_context = has_class_context && class_member_count > 0;

    if prefix_len >= MIN_GLOBAL_PREFIX_LEN
        && strong_item_count(&items) < STRONG_MATCH_TARGET
        && !suppress_globals_for_class_context
    {
        let global_items = fetch_global_symbols(conn, &prefix)?;
        merge_completion_items(&mut items, global_items, MAX_COMPLETION_ITEMS);

        if prefix_len >= MIN_ENGINE_GLOBAL_PREFIX_LEN
            && strong_item_count(&items) < STRONG_MATCH_TARGET
        {
            if let Some(engine_ctx) = engine_ctx.as_ref() {
                let engine_items = fetch_global_symbols(engine_ctx.conn, &prefix)?;
                merge_completion_items(
                    &mut items,
                    engine_items,
                    MAX_COMPLETION_ITEMS,
                );
            }
        }
    }

    let final_items = dedupe_completion_items(items);
    Ok(json!(final_items))
}

/// Complete members for the direct parent of the current class with Engine fallback.
/// 补全当前类直接父类的成员，并带上 Engine 兜底。
fn fetch_super_members_with_engine(
    ctx: &mut CompletionContext,
    engine_ctx: Option<&mut CompletionContext>,
    current_class: &str,
    prefix: Option<String>,
    buffer_inheritance: &BufferInheritanceMap,
    memory_cache: Option<Arc<Mutex<CompletionCache>>>,
    persistent_cache: Option<Arc<Mutex<Connection>>>,
) -> Result<Vec<Value>> {
    let Some(parent_class) = direct_parent_class(ctx, current_class, buffer_inheritance)? else {
        return Ok(Vec::new());
    };

    fetch_members_with_engine(
        ctx,
        engine_ctx,
        &parent_class,
        prefix,
        buffer_inheritance,
        memory_cache,
        persistent_cache,
        Some(current_class),
        true,
        false,
    )
}

/// Fetch members from the project DB and extend them with Engine parent-chain members.
/// 先查项目 DB 的成员，再补上 Engine 父类链的成员。
fn fetch_members_with_engine(
    ctx: &mut CompletionContext,
    engine_ctx: Option<&mut CompletionContext>,
    class_name: &str,
    prefix: Option<String>,
    buffer_inheritance: &BufferInheritanceMap,
    memory_cache: Option<Arc<Mutex<CompletionCache>>>,
    persistent_cache: Option<Arc<Mutex<Connection>>>,
    accessor_class: Option<&str>,
    assume_engine_subclass_access: bool,
    declaration_context: bool,
) -> Result<Vec<Value>> {
    let mut items = fetch_members_recursive(
        ctx,
        class_name,
        prefix.clone(),
        memory_cache.clone(),
        persistent_cache.clone(),
        accessor_class,
        false,
        false,
        declaration_context,
    )?;

    for parent_name in direct_buffer_parents(buffer_inheritance, class_name) {
        let extra = fetch_members_recursive(
            ctx,
            &parent_name,
            prefix.clone(),
            memory_cache.clone(),
            persistent_cache.clone(),
            accessor_class,
            true,
            false,
            declaration_context,
        )?;

        merge_completion_items(&mut items, extra, MAX_COMPLETION_ITEMS);
    }

    let Some(engine_ctx) = engine_ctx else {
        return Ok(items);
    };

    let mut roots = collect_engine_member_roots(ctx, engine_ctx, class_name, buffer_inheritance)?;

    if assume_engine_subclass_access {
        let resolved = resolve_typedef(ctx, class_name)?;
        if ctx.class_ids_by_name(&resolved)?.is_empty()
            && !engine_ctx.class_ids_by_name(&resolved)?.is_empty()
            && !roots.iter().any(|(name, _)| name == &resolved)
        {
            roots.insert(0, (resolved, true));
        }
    }

    for (root_name, assume_subclass_access) in roots {
        let extra = fetch_members_recursive(
            engine_ctx,
            &root_name,
            prefix.clone(),
            None,
            None,
            accessor_class,
            assume_subclass_access,
            true,
            declaration_context,
        )?;

        merge_completion_items(&mut items, extra, MAX_COMPLETION_ITEMS);
    }

    Ok(items)
}

/// Collect Engine-side parent roots that are referenced by project classes but not indexed locally.
/// 收集项目类引用到、但本地 DB 没有定义的 Engine 父类根节点。
fn collect_engine_member_roots(
    ctx: &mut CompletionContext,
    engine_ctx: &mut CompletionContext,
    class_name: &str,
    buffer_inheritance: &BufferInheritanceMap,
) -> Result<Vec<(String, bool)>> {
    let root_name = resolve_typedef(ctx, class_name)?;
    let mut queue = VecDeque::from([root_name.clone()]);
    let mut visited = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut roots = Vec::new();

    while let Some(current_name) = queue.pop_front() {
        let current_name = resolve_typedef(ctx, &current_name)?;

        if !visited.insert(current_name.clone()) {
            continue;
        }

        let class_ids = ctx.class_ids_by_name(&current_name)?;
        let in_engine = !engine_ctx.class_ids_by_name(&current_name)?.is_empty();

        if current_name == root_name {
            if class_ids.is_empty() && in_engine {
                roots.push((current_name.clone(), false));
            }
        } else if in_engine && seen_names.insert(current_name.clone()) {
            roots.push((current_name.clone(), true));
        }

        for class_id in class_ids {
            for (_, parent_name) in parent_classes(ctx.conn, class_id)? {
                let parent_name = clean_type(&parent_name);

                if !parent_name.is_empty() {
                    queue.push_back(parent_name);
                }
            }
        }

        for parent_name in direct_buffer_parents(buffer_inheritance, &current_name) {
            let parent_name = clean_type(&parent_name);

            if !parent_name.is_empty() {
                queue.push_back(parent_name);
            }
        }
    }

    Ok(roots)
}

/// Return the first direct parent class name for a class.
/// 返回某个类的第一个直接父类名。
fn direct_parent_class(
    ctx: &mut CompletionContext,
    class_name: &str,
    buffer_inheritance: &BufferInheritanceMap,
) -> Result<Option<String>> {
    for class_id in ctx.class_ids_by_name(class_name)? {
        for (_, parent_name) in parent_classes(ctx.conn, class_id)? {
            let parent_name = clean_type(&parent_name);

            if !parent_name.is_empty() {
                return Ok(Some(parent_name));
            }
        }
    }

    Ok(direct_buffer_parents(buffer_inheritance, class_name)
        .into_iter()
        .next())
}

// -----------------------------------------------------------------------------
// Cursor analysis
// -----------------------------------------------------------------------------

struct MemberCompletionRequest<'a> {
    receiver: Node<'a>,
    prefix: Option<String>,
}

/// Find node around cursor.
/// 获取光标附近的 tree-sitter node。
fn cursor_node(root: Node, line: u32, character: u32) -> Option<Node> {
    let row = line as usize;
    let col = character as usize;

    root.descendant_for_point_range(
        Point::new(row, col.saturating_sub(1)),
        Point::new(row, col),
    )
}

/// Detect member completion after ., ->, or ::.
/// 判断是否是 .、->、:: 后面的成员补全。
fn member_completion_request<'a>(
    node: Node<'a>,
    content: &str,
) -> Option<MemberCompletionRequest<'a>> {
    let mut current = Some(node);

    while let Some(n) = current {
        match n.kind() {
            "." | "->" | "::" => {
                let receiver = previous_meaningful_sibling(n)?;
                return Some(MemberCompletionRequest {
                    receiver,
                    prefix: None,
                });
            }

            "field_expression" => {
                let receiver = n
                    .child_by_field_name("argument")
                    .or_else(|| n.child(0))?;

                let prefix = n
                    .child_by_field_name("field")
                    .map(|field| node_text(field, content).trim().to_string())
                    .filter(|text| text != "." && text != "->" && !text.is_empty());

                return Some(MemberCompletionRequest { receiver, prefix });
            }

            "qualified_identifier" => {
                let receiver = n.child_by_field_name("scope")?;
                let prefix = n
                    .child_by_field_name("name")
                    .map(|name| node_text(name, content).trim().to_string())
                    .filter(|text| !text.is_empty());

                return Some(MemberCompletionRequest { receiver, prefix });
            }

            "ERROR" => {
                if let Some(request) = member_request_from_error(n, content) {
                    return Some(request);
                }
            }

            _ => {}
        }

        current = n.parent();
    }

    None
}

/// Recover member completion from an ERROR node.
/// 从 ERROR 节点里恢复成员补全上下文。
fn member_request_from_error<'a>(
    node: Node<'a>,
    _content: &str,
) -> Option<MemberCompletionRequest<'a>> {
    for index in (0..node.child_count()).rev() {
        let child = node.child(index as u32)?;
        if matches!(child.kind(), "." | "->" | "::") {
            let receiver = previous_meaningful_sibling(child)?;
            return Some(MemberCompletionRequest {
                receiver,
                prefix: None,
            });
        }
    }

    None
}

/// Get typed prefix under cursor.
/// 获取当前正在输入的补全前缀。
fn completion_prefix(node: Node, content: &str) -> String {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" => {
            node_text(node, content).trim().to_string()
        }
        _ => String::new(),
    }
}

/// Extract a word-like prefix directly from buffer text near the cursor.
/// 直接从光标左侧文本提取单词前缀，覆盖关键字等非 identifier 场景。
fn text_prefix_at(content: &str, line: usize, character: usize) -> Option<String> {
    let line_text = content.lines().nth(line)?;
    let mut end = character.min(line_text.len());

    while end > 0 && !line_text.is_char_boundary(end) {
        end -= 1;
    }

    let before = &line_text[..end];
    let start = before
        .rfind(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .map(|index| index + 1)
        .unwrap_or(0);

    let prefix = before[start..].trim();
    if prefix.is_empty() {
        return None;
    }

    let first = prefix.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }

    Some(prefix.to_string())
}

fn apply_declaration_completion_edits(
    items: &mut [Value],
    content: &str,
    line: u32,
    character: u32,
    prefix: &str,
) {
    if prefix.is_empty() {
        return;
    }

    for item in items.iter_mut() {
        if item.get("kind").and_then(Value::as_i64) != Some(2) {
            continue;
        }

        let Some(label) = item.get("label").and_then(Value::as_str) else {
            continue;
        };
        let Some(insert_text) = item.get("insertText").and_then(Value::as_str) else {
            continue;
        };

        let start = declaration_completion_start(content, line as usize, character as usize, prefix, label, insert_text);
        item["textEdit"] = json!({
            "newText": insert_text,
            "range": {
                "start": {
                    "line": line,
                    "character": start as u32,
                },
                "end": {
                    "line": line,
                    "character": character,
                },
            },
        });
    }
}

fn declaration_completion_start(
    content: &str,
    line: usize,
    character: usize,
    prefix: &str,
    label: &str,
    insert_text: &str,
) -> usize {
    let Some(line_text) = content.lines().nth(line) else {
        return character.saturating_sub(prefix.len());
    };

    let mut end = character.min(line_text.len());
    while end > 0 && !line_text.is_char_boundary(end) {
        end -= 1;
    }

    let before = &line_text[..end];
    let prefix_start = end.saturating_sub(prefix.len());
    let mut start = prefix_start;
    let before_prefix = &before[..prefix_start.min(before.len())];

    if let Some(head) = insert_text.split(label).next() {
        let trimmed_head = head.trim_end();
        if !trimmed_head.is_empty() {
            if let Some(pos) = before_prefix.rfind(trimmed_head) {
                start = pos;
            } else {
                if let Some(stripped) = trimmed_head.strip_prefix("virtual ") {
                    let stripped = stripped.trim_end();
                    if !stripped.is_empty() {
                        if let Some(pos) = before_prefix.rfind(stripped) {
                            start = pos;
                        }
                    }
                }

                if trimmed_head.starts_with("virtual ") {
                    if let Some(pos) = rfind_word(before_prefix, "virtual") {
                        start = start.min(pos);
                    }
                }
            }
        }
    }

    start
}

fn rfind_word(text: &str, needle: &str) -> Option<usize> {
    let mut search_from = text.len();

    while search_from >= needle.len() {
        let haystack = &text[..search_from];
        let pos = haystack.rfind(needle)?;
        let end = pos + needle.len();
        let before_ok = pos == 0
            || !text[..pos]
                .chars()
                .next_back()
                .map(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                .unwrap_or(false);
        let after_ok = end >= text.len()
            || !text[end..]
                .chars()
                .next()
                .map(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                .unwrap_or(false);

        if before_ok && after_ok {
            return Some(pos);
        }

        if pos == 0 {
            break;
        }
        search_from = pos;
    }

    None
}

/// Previous non-comment sibling.
/// 上一个有意义的 sibling。
fn previous_meaningful_sibling(node: Node) -> Option<Node> {
    let mut current = node.prev_sibling();

    while let Some(n) = current {
        if !matches!(n.kind(), "comment" | "\n" | "\r") {
            return Some(n);
        }

        current = n.prev_sibling();
    }

    None
}

// -----------------------------------------------------------------------------
// Type resolution
// -----------------------------------------------------------------------------

/// Resolve expression type for member completion.
/// 解析表达式类型，用于成员补全。
fn resolve_expression_type(
    ctx: &mut CompletionContext,
    node: Node,
    root: Node,
    content: &str,
    cursor_row: usize,
) -> Result<Option<String>> {
    match node.kind() {
        "this" => Ok(enclosing_class(node, content)),

        "identifier" | "field_identifier" | "type_identifier" | "namespace_identifier" => {
            let name = node_text(node, content).trim();

            if name == "this" {
                return Ok(enclosing_class(node, content));
            }

            if let Some(ty) = infer_variable_type(ctx, name, root, content, cursor_row)? {
                return Ok(Some(ty));
            }

            if let Some(class_name) = enclosing_class(node, content) {
                if let Some(return_type) = find_member_return_type(ctx, &class_name, name)? {
                    return Ok(Some(return_type));
                }
            }

            if is_known_type(ctx, name)? {
                return Ok(Some(name.to_string()));
            }

            Ok(None)
        }

        "qualified_identifier" => {
            let text = node_text(node, content).trim();

            if is_known_type(ctx, text)? {
                return Ok(Some(text.to_string()));
            }

            if let Some((class_name, member_name)) = text.rsplit_once("::") {
                return find_member_return_type(ctx, class_name, member_name);
            }

            Ok(None)
        }

        "call_expression" => {
            let Some(function) = node.child_by_field_name("function") else {
                return Ok(None);
            };

            if let Some(ty) = resolve_special_call_type(ctx, function, root, content, cursor_row)? {
                return Ok(Some(ty));
            }

            match function.kind() {
                "field_expression" => {
                    let object = function.child_by_field_name("argument").or_else(|| function.child(0));
                    let field = function.child_by_field_name("field");

                    if let (Some(object), Some(field)) = (object, field) {
                        if let Some(object_type) =
                            resolve_expression_type(ctx, object, root, content, cursor_row)?
                        {
                            return find_member_return_type(
                                ctx,
                                &object_type,
                                node_text(field, content).trim(),
                            );
                        }
                    }

                    Ok(None)
                }

                _ => {
                    let name = node_text(function, content).trim();

                    if let Some((class_name, method_name)) = name.rsplit_once("::") {
                        return find_member_return_type(ctx, class_name, method_name);
                    }

                    if let Some(class_name) = enclosing_class(node, content) {
                        return find_member_return_type(ctx, &class_name, name);
                    }

                    Ok(None)
                }
            }
        }

        "field_expression" => {
            let object = node.child_by_field_name("argument").or_else(|| node.child(0));
            let field = node.child_by_field_name("field");

            if let (Some(object), Some(field)) = (object, field) {
                if let Some(object_type) =
                    resolve_expression_type(ctx, object, root, content, cursor_row)?
                {
                    return find_member_return_type(ctx, &object_type, node_text(field, content));
                }
            }

            Ok(None)
        }

        "subscript_expression" => {
            let object = node.child_by_field_name("argument").or_else(|| node.child(0));

            if let Some(object) = object {
                if let Some(object_type) =
                    resolve_expression_type(ctx, object, root, content, cursor_row)?
                {
                    return Ok(Some(unwrap_container_type(&object_type)));
                }
            }

            Ok(None)
        }

        "parenthesized_expression" | "pointer_expression" | "reference_declarator" => {
            for child in node_children(node) {
                if !matches!(child.kind(), "(" | ")" | "*" | "&") {
                    return resolve_expression_type(ctx, child, root, content, cursor_row);
                }
            }

            Ok(None)
        }

        _ => Ok(None),
    }
}

type BufferInheritanceMap = HashMap<String, Vec<String>>;

/// Resolve expression type with optional Engine DB fallback.
/// 带可选 Engine DB 兜底的表达式类型解析。
fn resolve_expression_type_with_engine(
    ctx: &mut CompletionContext,
    engine_ctx: Option<&mut CompletionContext>,
    node: Node,
    buffer_inheritance: &BufferInheritanceMap,
    root: Node,
    content: &str,
    cursor_row: usize,
) -> Result<Option<String>> {
    let mut engine_ctx = engine_ctx;

    match node.kind() {
        "this" => Ok(enclosing_class(node, content)),

        "identifier" | "field_identifier" | "type_identifier" | "namespace_identifier" => {
            let name = node_text(node, content).trim();

            if name == "this" {
                return Ok(enclosing_class(node, content));
            }

            if let Some(ty) = infer_variable_type(ctx, name, root, content, cursor_row)? {
                return Ok(Some(ty));
            }

            if let Some(class_name) = enclosing_class(node, content) {
                if let Some(return_type) = find_member_return_type_with_engine(
                    ctx,
                    engine_ctx.as_deref_mut(),
                    &class_name,
                    name,
                    buffer_inheritance,
                )? {
                    return Ok(Some(return_type));
                }
            }

            if is_known_type_with_engine(ctx, engine_ctx.as_deref_mut(), name)? {
                return Ok(Some(name.to_string()));
            }

            Ok(None)
        }

        "qualified_identifier" => {
            let text = node_text(node, content).trim();

            if is_known_type_with_engine(ctx, engine_ctx.as_deref_mut(), text)? {
                return Ok(Some(text.to_string()));
            }

            if let Some((class_name, member_name)) = text.rsplit_once("::") {
                return find_member_return_type_with_engine(
                    ctx,
                    engine_ctx.as_deref_mut(),
                    class_name,
                    member_name,
                    buffer_inheritance,
                );
            }

            Ok(None)
        }

        "call_expression" => {
            let Some(function) = node.child_by_field_name("function") else {
                return Ok(None);
            };

            if let Some(ty) = resolve_special_call_type_with_engine(
                ctx,
                engine_ctx.as_deref_mut(),
                function,
                buffer_inheritance,
                root,
                content,
                cursor_row,
            )? {
                return Ok(Some(ty));
            }

            match function.kind() {
                "field_expression" => {
                    let object = function.child_by_field_name("argument").or_else(|| function.child(0));
                    let field = function.child_by_field_name("field");

                    if let (Some(object), Some(field)) = (object, field) {
                        if let Some(object_type) = resolve_expression_type_with_engine(
                            ctx,
                            engine_ctx.as_deref_mut(),
                            object,
                            buffer_inheritance,
                            root,
                            content,
                            cursor_row,
                        )? {
                            return find_member_return_type_with_engine(
                                ctx,
                                engine_ctx.as_deref_mut(),
                                &object_type,
                                node_text(field, content).trim(),
                                buffer_inheritance,
                            );
                        }
                    }

                    Ok(None)
                }

                _ => {
                    let name = node_text(function, content).trim();

                    if let Some((class_name, method_name)) = name.rsplit_once("::") {
                        return find_member_return_type_with_engine(
                            ctx,
                            engine_ctx.as_deref_mut(),
                            class_name,
                            method_name,
                            buffer_inheritance,
                        );
                    }

                    if let Some(class_name) = enclosing_class(node, content) {
                        return find_member_return_type_with_engine(
                            ctx,
                            engine_ctx.as_deref_mut(),
                            &class_name,
                            name,
                            buffer_inheritance,
                        );
                    }

                    Ok(None)
                }
            }
        }

        "field_expression" => {
            let object = node.child_by_field_name("argument").or_else(|| node.child(0));
            let field = node.child_by_field_name("field");

            if let (Some(object), Some(field)) = (object, field) {
                if let Some(object_type) = resolve_expression_type_with_engine(
                    ctx,
                    engine_ctx.as_deref_mut(),
                    object,
                    buffer_inheritance,
                    root,
                    content,
                    cursor_row,
                )? {
                    return find_member_return_type_with_engine(
                        ctx,
                        engine_ctx.as_deref_mut(),
                        &object_type,
                        node_text(field, content).trim(),
                        buffer_inheritance,
                    );
                }
            }

            Ok(None)
        }

        "subscript_expression" => {
            let object = node.child_by_field_name("argument").or_else(|| node.child(0));

            if let Some(object) = object {
                if let Some(object_type) = resolve_expression_type_with_engine(
                    ctx,
                    engine_ctx.as_deref_mut(),
                    object,
                    buffer_inheritance,
                    root,
                    content,
                    cursor_row,
                )? {
                    return Ok(Some(unwrap_container_type(&object_type)));
                }
            }

            Ok(None)
        }

        "parenthesized_expression" | "pointer_expression" | "reference_declarator" => {
            for child in node_children(node) {
                if !matches!(child.kind(), "(" | ")" | "*" | "&") {
                    return resolve_expression_type_with_engine(
                        ctx,
                        engine_ctx.as_deref_mut(),
                        child,
                        buffer_inheritance,
                        root,
                        content,
                        cursor_row,
                    );
                }
            }

            Ok(None)
        }

        _ => resolve_expression_type(ctx, node, root, content, cursor_row),
    }
}

/// Resolve known Unreal factory/cast calls with optional Engine fallback.
/// 带可选 Engine 兜底地解析 Unreal 常见工厂函数或 Cast 调用。
fn resolve_special_call_type_with_engine(
    ctx: &mut CompletionContext,
    engine_ctx: Option<&mut CompletionContext>,
    function: Node,
    buffer_inheritance: &BufferInheritanceMap,
    root: Node,
    content: &str,
    cursor_row: usize,
) -> Result<Option<String>> {
    let text = node_text(function, content).trim();

    if let Some(template_type) = extract_template_call_type(text) {
        let function_name = text.split('<').next().unwrap_or("");

        if matches!(
            function_name,
            "Cast"
                | "CastChecked"
                | "ExactCast"
                | "NewObject"
                | "CreateWidget"
                | "CreateDefaultSubobject"
        ) {
            return Ok(Some(template_type));
        }
    }

    resolve_expression_type_with_engine(
        ctx,
        engine_ctx,
        function,
        buffer_inheritance,
        root,
        content,
        cursor_row,
    )
}

/// Find a member return type with optional Engine parent-chain fallback.
/// 查找成员返回类型，并支持 Engine 父类链兜底。
fn find_member_return_type_with_engine(
    ctx: &mut CompletionContext,
    engine_ctx: Option<&mut CompletionContext>,
    class_name: &str,
    member_name: &str,
    buffer_inheritance: &BufferInheritanceMap,
) -> Result<Option<String>> {
    if let Some(ty) = find_member_return_type(ctx, class_name, member_name)? {
        return Ok(Some(ty));
    }

    for parent_name in direct_buffer_parents(buffer_inheritance, class_name) {
        if let Some(ty) = find_member_return_type(ctx, &parent_name, member_name)? {
            return Ok(Some(ty));
        }
    }

    let Some(engine_ctx) = engine_ctx else {
        return Ok(None);
    };

    let resolved = resolve_typedef(ctx, class_name)?;
    let mut roots = collect_engine_member_roots(ctx, engine_ctx, &resolved, buffer_inheritance)?;

    if ctx.class_ids_by_name(&resolved)?.is_empty()
        && !engine_ctx.class_ids_by_name(&resolved)?.is_empty()
        && !roots.iter().any(|(name, _)| name == &resolved)
    {
        roots.insert(0, (resolved, false));
    }

    for (root_name, _) in roots {
        if let Some(ty) = find_member_return_type(engine_ctx, &root_name, member_name)? {
            return Ok(Some(ty));
        }
    }

    Ok(None)
}

/// Check whether a type exists in either the project DB or the Engine DB.
/// 检查某个类型是否存在于项目 DB 或 Engine DB。
fn is_known_type_with_engine(
    ctx: &mut CompletionContext,
    engine_ctx: Option<&mut CompletionContext>,
    name: &str,
) -> Result<bool> {
    if is_known_type(ctx, name)? {
        return Ok(true);
    }

    let Some(engine_ctx) = engine_ctx else {
        return Ok(false);
    };

    is_known_type(engine_ctx, name)
}

/// Resolve known Unreal factory/cast calls.
/// 解析 Unreal 常见工厂函数或 Cast 调用的返回类型。
fn resolve_special_call_type(
    ctx: &mut CompletionContext,
    function: Node,
    root: Node,
    content: &str,
    cursor_row: usize,
) -> Result<Option<String>> {
    let text = node_text(function, content).trim();

    if let Some(template_type) = extract_template_call_type(text) {
        let function_name = text.split('<').next().unwrap_or("");

        if matches!(
            function_name,
            "Cast"
                | "CastChecked"
                | "ExactCast"
                | "NewObject"
                | "CreateWidget"
                | "CreateDefaultSubobject"
        ) {
            return Ok(Some(template_type));
        }
    }

    resolve_expression_type(ctx, function, root, content, cursor_row)
}

/// Infer local variable or parameter type.
/// 推断局部变量或参数类型。
fn infer_variable_type(
    ctx: &mut CompletionContext,
    target_name: &str,
    root: Node,
    content: &str,
    cursor_row: usize,
) -> Result<Option<String>> {
    let mut best: Option<(usize, String)> = None;

    scan_declarations(root, content, cursor_row, target_name, &mut best);

    if let Some((_, ty)) = best {
        if ty == "auto" {
            if let Some(ty) =
                infer_from_declaration_initializer(target_name, root, content, cursor_row)
            {
                return Ok(Some(ty));
            }

            return infer_from_assignment_text(ctx, target_name, root, content, cursor_row);
        }

        return Ok(Some(clean_type(&ty)));
    }

    infer_from_assignment_text(ctx, target_name, root, content, cursor_row)
}

/// Recursively scan declarations before cursor.
/// 递归扫描光标前的声明。
fn scan_declarations(
    node: Node,
    content: &str,
    cursor_row: usize,
    target_name: &str,
    best: &mut Option<(usize, String)>,
) {
    if matches!(
        node.kind(),
        "declaration" | "field_declaration" | "parameter_declaration"
    ) {
        if let (Some(type_node), Some(decl_node)) = (
            node.child_by_field_name("type"),
            node.child_by_field_name("declarator"),
        ) {
            if declaration_contains_name(decl_node, content, target_name) {
                let row = node.start_position().row;

                if row <= cursor_row && best.as_ref().map(|(r, _)| row >= *r).unwrap_or(true) {
                    *best = Some((row, node_text(type_node, content).trim().to_string()));
                }
            }
        }
    }

    for child in node_children(node) {
        scan_declarations(child, content, cursor_row, target_name, best);
    }
}

/// Check if declarator contains target variable name.
/// 判断 declarator 是否包含目标变量名。
fn declaration_contains_name(node: Node, content: &str, target_name: &str) -> bool {
    if matches!(node.kind(), "identifier" | "field_identifier") {
        return node_text(node, content).trim() == target_name;
    }

    for child in node_children(node) {
        if declaration_contains_name(child, content, target_name) {
            return true;
        }
    }

    false
}

fn infer_from_declaration_initializer(
    target_name: &str,
    root: Node,
    content: &str,
    cursor_row: usize,
) -> Option<String> {
    let mut result = None;
    scan_declaration_initializers(root, content, cursor_row, target_name, &mut result);
    result
}

fn scan_declaration_initializers(
    node: Node,
    content: &str,
    cursor_row: usize,
    target_name: &str,
    result: &mut Option<String>,
) {
    if node.start_position().row > cursor_row || result.is_some() {
        return;
    }

    if matches!(node.kind(), "declaration" | "init_declarator") {
        let text = node_text(node, content);

        if declaration_text_names_variable(text, target_name) {
            if let Some(initializer) = initializer_text(text) {
                *result = infer_type_from_value_text(initializer);
                return;
            }
        }
    }

    for child in node_children(node) {
        scan_declaration_initializers(child, content, cursor_row, target_name, result);
    }
}

fn declaration_text_names_variable(text: &str, target_name: &str) -> bool {
    let Some(before_equal) = text.split('=').next() else {
        return false;
    };

    before_equal
        .split(|ch: char| !matches!(ch, '_' | 'A'..='Z' | 'a'..='z' | '0'..='9'))
        .any(|part| part == target_name)
}

fn initializer_text(text: &str) -> Option<&str> {
    text.split_once('=')
        .map(|(_, right)| right.trim().trim_end_matches(';').trim())
        .filter(|text| !text.is_empty())
}

/// Infer type from assignment expression text.
/// 根据赋值表达式文本推断类型。
fn infer_from_assignment_text(
    _ctx: &mut CompletionContext,
    target_name: &str,
    root: Node,
    content: &str,
    cursor_row: usize,
) -> Result<Option<String>> {
    let mut result = None;
    scan_assignment_text(root, content, cursor_row, target_name, &mut result);
    Ok(result)
}

/// Scan assignment expressions for simple known patterns.
/// 扫描赋值表达式中的简单已知模式。
fn scan_assignment_text(
    node: Node,
    content: &str,
    cursor_row: usize,
    target_name: &str,
    result: &mut Option<String>,
) {
    if node.start_position().row > cursor_row || result.is_some() {
        return;
    }

    if node.kind() == "assignment_expression" {
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");

        if let (Some(left), Some(right)) = (left, right) {
            if node_text(left, content).trim() == target_name {
                *result = infer_type_from_value_text(node_text(right, content));
                return;
            }
        }
    }

    for child in node_children(node) {
        scan_assignment_text(child, content, cursor_row, target_name, result);
    }
}

/// Infer type from common expression text.
/// 从常见表达式文本推断类型。
fn infer_type_from_value_text(text: &str) -> Option<String> {
    let text = text.trim();

    if let Some(ty) = extract_template_call_type(text) {
        return Some(ty);
    }

    let head = text.split('(').next()?.trim();

    if let Some(known) = known_call_return_type(head) {
        return Some(known.to_string());
    }

    if head.contains("::") {
        return Some(clean_type(head.rsplit_once("::")?.0));
    }

    Some(clean_type(head)).filter(|s| !s.is_empty())
}

fn known_call_return_type(function_name: &str) -> Option<&'static str> {
    match function_name.trim() {
        "GetWorld" => Some("UWorld"),
        "GetGameInstance" => Some("UGameInstance"),
        "GetOwner" => Some("AActor"),
        "GetController" => Some("AController"),
        "GetPawn" => Some("APawn"),
        "GetPlayerController" => Some("APlayerController"),
        "GetComponentByClass" => Some("UActorComponent"),
        "FindComponentByClass" => Some("UActorComponent"),
        _ => None,
    }
}

struct DeclaratorIdentity {
    name: String,
    is_function: bool,
    has_scope: bool,
}

/// Collect visible local variables and parameters for ordinary completion.
/// 为普通补全收集当前可见的局部变量和参数。
fn collect_local_completion_items(
    cursor_node: Node,
    content: &str,
    cursor_row: usize,
    cursor_col: usize,
    prefix: &str,
) -> Vec<Value> {
    if prefix.is_empty() {
        return Vec::new();
    }

    let Some(scope_root) = enclosing_callable(cursor_node) else {
        return Vec::new();
    };

    let cursor_point = Point::new(cursor_row, cursor_col);
    let mut items = Vec::new();
    let mut seen = HashSet::new();

    if let Some(declarator) = scope_root.child_by_field_name("declarator") {
        collect_parameter_completion_items(declarator, content, prefix, &mut seen, &mut items);
    }

    collect_visible_local_declarations(
        scope_root,
        cursor_point,
        content,
        prefix,
        &mut seen,
        &mut items,
    );

    items
}

/// Collect current-buffer free functions and unsaved type symbols.
/// 收集当前 buffer 里的自由函数和未落盘类型符号。
fn collect_buffer_symbol_items(
    root: Node,
    content: &str,
    cursor_row: usize,
    prefix: &str,
) -> Vec<Value> {
    if prefix.is_empty() {
        return Vec::new();
    }

    let mut items = Vec::new();
    let mut seen = HashSet::new();

    collect_buffer_symbols_recursive(root, content, cursor_row, &mut seen, &mut items, prefix);

    items
}

/// Merge completion items while preserving existing order and deduplicating by label.
/// 合并补全项，保持已有顺序，并按 label 去重。
fn merge_completion_items(target: &mut Vec<Value>, extra: Vec<Value>, limit: usize) {
    let mut seen = target
        .iter()
        .filter_map(|item| item.get("label").and_then(Value::as_str))
        .map(|label| label.to_string())
        .collect::<HashSet<_>>();

    for item in extra {
        if target.len() >= limit {
            break;
        }

        let Some(label) = item.get("label").and_then(Value::as_str) else {
            continue;
        };

        if seen.insert(label.to_string()) {
            target.push(item);
        }
    }
}

fn collect_parameter_completion_items(
    node: Node,
    content: &str,
    prefix: &str,
    seen: &mut HashSet<String>,
    items: &mut Vec<Value>,
) {
    if node.kind() == "parameter_declaration" {
        append_declaration_completion_items(node, content, prefix, seen, items, "parameter", 0);
        return;
    }

    for child in node_children(node) {
        collect_parameter_completion_items(child, content, prefix, seen, items);
    }
}

fn collect_visible_local_declarations(
    node: Node,
    cursor_point: Point,
    content: &str,
    prefix: &str,
    seen: &mut HashSet<String>,
    items: &mut Vec<Value>,
) {
    for child in node_children(node) {
        if point_is_before(cursor_point, child.start_position()) {
            continue;
        }

        if child.kind() == "declaration" {
            append_declaration_completion_items(child, content, prefix, seen, items, "local", 5);
        }

        if node_contains_point(child, cursor_point)
            && !matches!(
                child.kind(),
                "class_specifier"
                    | "struct_specifier"
                    | "unreal_reflected_class_declaration"
                    | "unreal_reflected_struct_declaration"
                    | "enum_specifier"
            )
        {
            collect_visible_local_declarations(child, cursor_point, content, prefix, seen, items);
        }
    }
}

fn append_declaration_completion_items(
    node: Node,
    content: &str,
    prefix: &str,
    seen: &mut HashSet<String>,
    items: &mut Vec<Value>,
    description: &str,
    rank_base: usize,
) {
    let Some(declarator) = node.child_by_field_name("declarator") else {
        return;
    };

    let type_text = completion_decl_type(node, content);
    let mut identities = Vec::new();
    collect_declarator_identities(declarator, content, false, false, &mut identities);

    for identity in identities {
        if identity.is_function || identity.has_scope || identity.name.is_empty() {
            continue;
        }

        let match_rank = completion_match_rank(&identity.name, prefix);
        if match_rank == COMPLETION_MATCH_NONE || !seen.insert(identity.name.clone()) {
            continue;
        }

        let detail = if type_text.is_empty() {
            description.to_string()
        } else {
            format!("{} {}", type_text, description)
        };

        items.push(json!({
            "label": identity.name,
            "kind": 6,
            "detail": detail,
            "filterText": identity.name,
            "insertText": identity.name,
            "sortText": completion_sort_text(rank_base + match_rank, 6, &identity.name),
            "score_offset": completion_score_offset(match_rank),
        }));
    }
}

fn collect_buffer_symbols_recursive(
    node: Node,
    content: &str,
    cursor_row: usize,
    seen: &mut HashSet<String>,
    items: &mut Vec<Value>,
    prefix: &str,
) {
    for child in node_children(node) {
        if child.start_position().row > cursor_row {
            continue;
        }

        match child.kind() {
            "class_specifier"
            | "struct_specifier"
            | "enum_specifier"
            | "unreal_reflected_class_declaration"
            | "unreal_reflected_struct_declaration"
            | "unreal_reflected_enum_declaration" => {
                append_buffer_type_item(child, content, prefix, seen, items);
                collect_buffer_symbols_recursive(child, content, cursor_row, seen, items, prefix);
            }

            "function_definition" => {
                append_buffer_function_item(child, content, prefix, seen, items);
            }

            "declaration" | "field_declaration" | "unreal_function_declaration" => {
                append_buffer_function_item(child, content, prefix, seen, items);
            }

            "namespace_definition" => {
                collect_buffer_symbols_recursive(child, content, cursor_row, seen, items, prefix);
            }

            _ => {
                collect_buffer_symbols_recursive(child, content, cursor_row, seen, items, prefix);
            }
        }
    }
}

fn append_buffer_type_item(
    node: Node,
    content: &str,
    prefix: &str,
    seen: &mut HashSet<String>,
    items: &mut Vec<Value>,
) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };

    let label = clean_type(node_text(name, content));
    if label.is_empty() {
        return;
    }

    let match_rank = completion_match_rank(&label, prefix);
    if match_rank == COMPLETION_MATCH_NONE || !seen.insert(label.clone()) {
        return;
    }

    let (detail, kind) = match node.kind() {
        "enum_specifier" | "unreal_reflected_enum_declaration" => ("enum", 13),
        "struct_specifier" | "unreal_reflected_struct_declaration" => ("struct", 7),
        _ => ("class", 7),
    };

    items.push(json!({
        "label": label,
        "kind": kind,
        "detail": detail,
        "filterText": label,
        "insertText": label,
        "sortText": completion_sort_text(200 + match_rank, kind, &label),
        "score_offset": completion_score_offset(match_rank),
    }));
}

fn append_buffer_function_item(
    node: Node,
    content: &str,
    prefix: &str,
    seen: &mut HashSet<String>,
    items: &mut Vec<Value>,
) {
    let Some(declarator) = node.child_by_field_name("declarator") else {
        return;
    };

    let mut identities = Vec::new();
    collect_declarator_identities(declarator, content, false, false, &mut identities);

    let Some(identity) = identities
        .into_iter()
        .find(|identity| identity.is_function && !identity.has_scope)
    else {
        return;
    };

    let match_rank = completion_match_rank(&identity.name, prefix);
    if match_rank == COMPLETION_MATCH_NONE || !seen.insert(identity.name.clone()) {
        return;
    }

    let return_type = node
        .child_by_field_name("type")
        .map(|type_node| clean_type(node_text(type_node, content)))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "function".to_string());
    let params = find_descendant_by_kind(node, "parameter_list")
        .map(|params_node| node_text(params_node, content).to_string());
    let detail = function_completion_detail(Some(&return_type), params.as_deref(), None);
    let insert_text = function_snippet_text(&identity.name, params.as_deref());

    items.push(json!({
        "label": identity.name,
        "kind": 2,
        "detail": detail,
        "filterText": identity.name,
        "insertText": insert_text,
        "insertTextFormat": 2,
        "sortText": completion_sort_text(250 + match_rank, 2, &identity.name),
        "score_offset": completion_score_offset(match_rank),
    }));
}

fn completion_decl_type(node: Node, content: &str) -> String {
    let declared = node
        .child_by_field_name("type")
        .map(|type_node| clean_type(node_text(type_node, content)))
        .unwrap_or_default();

    if declared == "auto" {
        let text = node_text(node, content);
        if let Some(initializer) = initializer_text(text) {
            if let Some(inferred) = infer_type_from_value_text(initializer) {
                return inferred;
            }
        }
    }

    declared
}

fn collect_declarator_identities(
    node: Node,
    content: &str,
    is_function: bool,
    has_scope: bool,
    out: &mut Vec<DeclaratorIdentity>,
) {
    match node.kind() {
        "identifier" | "field_identifier" => {
            let name = node_text(node, content).trim();
            if !name.is_empty() {
                out.push(DeclaratorIdentity {
                    name: name.to_string(),
                    is_function,
                    has_scope,
                });
            }
        }

        "qualified_identifier" => {
            let name = node
                .child_by_field_name("name")
                .map(|child| node_text(child, content).trim().to_string())
                .unwrap_or_default();

            if !name.is_empty() {
                out.push(DeclaratorIdentity {
                    name,
                    is_function,
                    has_scope: true,
                });
            }
        }

        "function_declarator" => {
            if let Some(next) = node.child_by_field_name("declarator") {
                collect_declarator_identities(next, content, true, has_scope, out);
            }
        }

        "init_declarator"
        | "pointer_declarator"
        | "reference_declarator"
        | "array_declarator"
        | "parenthesized_declarator" => {
            if let Some(next) = node.child_by_field_name("declarator") {
                collect_declarator_identities(next, content, is_function, has_scope, out);
                return;
            }

            for child in node_children(node) {
                collect_declarator_identities(child, content, is_function, has_scope, out);
            }
        }

        _ => {
            for child in node_children(node) {
                collect_declarator_identities(child, content, is_function, has_scope, out);
            }
        }
    }
}

fn enclosing_callable(node: Node) -> Option<Node> {
    let mut current = Some(node);

    while let Some(node) = current {
        if matches!(
            node.kind(),
            "function_definition" | "unreal_function_definition" | "lambda_expression"
        ) {
            return Some(node);
        }

        current = node.parent();
    }

    None
}

fn is_member_declaration_context_text(before_cursor: &str) -> bool {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum BlockKind {
        TypeLike,
        Other,
    }

    let mut stack = Vec::new();

    for (index, ch) in before_cursor.char_indices() {
        match ch {
            '{' => {
                let header_start = before_cursor[..index]
                    .rfind(|ch| matches!(ch, ';' | '{' | '}'))
                    .map(|found| found + 1)
                    .unwrap_or(0);
                let header = before_cursor[header_start..index].trim();
                let is_type_like = header.contains("class ")
                    || header.contains("struct ")
                    || header.contains("enum ")
                    || header.starts_with("namespace")
                    || header.contains(" namespace ");
                stack.push(if is_type_like {
                    BlockKind::TypeLike
                } else {
                    BlockKind::Other
                });
            }
            '}' => {
                stack.pop();
            }
            _ => {}
        }
    }

    matches!(stack.last(), Some(BlockKind::TypeLike))
}

fn node_contains_point(node: Node, point: Point) -> bool {
    !point_is_before(point, node.start_position()) && !point_is_before(node.end_position(), point)
}

fn point_is_before(left: Point, right: Point) -> bool {
    left.row < right.row || (left.row == right.row && left.column < right.column)
}

fn build_buffer_inheritance_map(root: Node, content: &str) -> BufferInheritanceMap {
    let mut map = HashMap::new();
    collect_buffer_inheritance(root, content, &mut map);
    map
}

fn collect_buffer_inheritance(node: Node, content: &str, map: &mut BufferInheritanceMap) {
    if matches!(
        node.kind(),
        "class_specifier"
            | "struct_specifier"
            | "unreal_class_declaration"
            | "unreal_struct_declaration"
            | "unreal_reflected_class_declaration"
            | "unreal_reflected_struct_declaration"
    ) {
        if let Some(name_node) = node.child_by_field_name("name") {
            let class_name = clean_type(node_text(name_node, content));
            let parents = base_class_names(node, content);

            if !class_name.is_empty() && !parents.is_empty() {
                map.insert(class_name, parents);
            }
        }
    }

    for child in node_children(node) {
        collect_buffer_inheritance(child, content, map);
    }
}

fn base_class_names(node: Node, content: &str) -> Vec<String> {
    let mut parents = Vec::new();
    let mut seen = HashSet::new();

    for child in node_children(node) {
        if child.kind() == "base_class_clause" {
            collect_base_class_names(child, content, &mut seen, &mut parents);
        }
    }

    parents
}

fn collect_base_class_names(
    node: Node,
    content: &str,
    seen: &mut HashSet<String>,
    out: &mut Vec<String>,
) {
    match node.kind() {
        "type_identifier" | "qualified_identifier" | "template_type" => {
            let name = clean_type(node_text(node, content));

            if !name.is_empty() && seen.insert(name.clone()) {
                out.push(name);
            }
            return;
        }

        _ => {}
    }

    for child in node_children(node) {
        collect_base_class_names(child, content, seen, out);
    }
}

fn direct_buffer_parents(buffer_inheritance: &BufferInheritanceMap, class_name: &str) -> Vec<String> {
    buffer_inheritance
        .get(&clean_type(class_name))
        .cloned()
        .unwrap_or_default()
}

fn split_identifier_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut prev_is_lower_or_digit = false;

    for ch in text.chars() {
        if !(ch == '_' || ch.is_ascii_alphanumeric()) {
            if !current.is_empty() {
                words.push(current.to_ascii_lowercase());
                current.clear();
            }
            prev_is_lower_or_digit = false;
            continue;
        }

        if ch == '_' {
            if !current.is_empty() {
                words.push(current.to_ascii_lowercase());
                current.clear();
            }
            prev_is_lower_or_digit = false;
            continue;
        }

        if ch.is_ascii_uppercase() && prev_is_lower_or_digit && !current.is_empty() {
            words.push(current.to_ascii_lowercase());
            current.clear();
        }

        current.push(ch);
        prev_is_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }

    if !current.is_empty() {
        words.push(current.to_ascii_lowercase());
    }

    words
}

fn compact_identifier(text: &str) -> String {
    text.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn has_multi_word_head_anchor(candidate_words: &[String], prefix_words: &[String]) -> bool {
    if prefix_words.len() < 2 {
        return true;
    }

    let Some(first_prefix_word) = prefix_words.first() else {
        return true;
    };

    candidate_words
        .first()
        .map(|word| word.starts_with(first_prefix_word))
        .unwrap_or(false)
}

fn ordered_word_match_rank(candidate_words: &[String], prefix_words: &[String]) -> Option<usize> {
    if candidate_words.is_empty() || prefix_words.len() < 2 {
        return None;
    }

    let mut best_rank: Option<usize> = None;

    for start in 0..candidate_words.len() {
        if !candidate_words[start].starts_with(&prefix_words[0]) {
            continue;
        }

        let mut prev_index = start;
        let mut gaps = 0usize;
        let mut matched = true;

        for prefix_word in prefix_words.iter().skip(1) {
            let mut found_index = None;

            for candidate_index in (prev_index + 1)..candidate_words.len() {
                if candidate_words[candidate_index].starts_with(prefix_word) {
                    found_index = Some(candidate_index);
                    break;
                }
            }

            let Some(next_index) = found_index else {
                matched = false;
                break;
            };

            gaps += next_index - prev_index - 1;
            prev_index = next_index;
        }

        if matched {
            let rank = start * 10 + gaps;
            best_rank = Some(best_rank.map_or(rank, |current| current.min(rank)));
        }
    }

    best_rank
}

fn completion_match_rank(candidate: &str, prefix: &str) -> usize {
    if prefix.is_empty() {
        return 0;
    }

    let candidate_lower = candidate.to_ascii_lowercase();
    let prefix_lower = prefix.to_ascii_lowercase();
    let prefix_words = split_identifier_words(prefix);

    if candidate_lower.starts_with(&prefix_lower) {
        return 0;
    }

    let words = split_identifier_words(candidate);
    let head_anchor = has_multi_word_head_anchor(&words, &prefix_words);
    if !words.is_empty() {
        let joined = words.join("");
        if joined.starts_with(&prefix_lower) {
            return 1;
        }

        for (index, word) in words.iter().enumerate() {
            if word.starts_with(&prefix_lower) {
                return 2 + index;
            }
        }

        if head_anchor {
            for start in 0..words.len() {
                let tail = words[start..].join("");
                if tail.starts_with(&prefix_lower) {
                    return 10 + start;
                }
            }

            if let Some(rank) = ordered_word_match_rank(&words, &prefix_words) {
                return 20 + rank;
            }
        }
    }

    if prefix_words.len() >= 2 && !head_anchor {
        return COMPLETION_MATCH_NONE;
    }

    if let Some(pos) = candidate_lower.find(&prefix_lower) {
        return 40 + pos;
    }

    let compact_candidate = compact_identifier(candidate);
    let compact_prefix = compact_identifier(prefix);
    if !compact_prefix.is_empty() {
        if compact_candidate.starts_with(&compact_prefix) {
            return 80;
        }

        if let Some(pos) = compact_candidate.find(&compact_prefix) {
            return 100 + pos;
        }
    }

    COMPLETION_MATCH_NONE
}

fn completion_score_offset(match_rank: usize) -> i64 {
    match match_rank {
        0 => 24,
        1 => 18,
        2..=19 => 10,
        20..=39 => 2,
        40..=79 => -8,
        _ => -14,
    }
}

fn strong_item_count(items: &[Value]) -> usize {
    items.iter()
        .filter(|item| {
            item.get("score_offset")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                >= STRONG_MATCH_SCORE_OFFSET
        })
        .count()
}

// -----------------------------------------------------------------------------
// Member fetch
// -----------------------------------------------------------------------------

/// Fetch members recursively through inheritance.
/// 递归继承链获取成员补全。
fn fetch_members_recursive(
    ctx: &mut CompletionContext,
    class_name: &str,
    prefix: Option<String>,
    memory_cache: Option<Arc<Mutex<CompletionCache>>>,
    persistent_cache: Option<Arc<Mutex<Connection>>>,
    accessor_class: Option<&str>,
    assume_subclass_access: bool,
    include_impl_members: bool,
    declaration_context: bool,
) -> Result<Vec<Value>> {
    let class_name = resolve_typedef(ctx, class_name)?;
    let prefix = prefix.unwrap_or_default();
    let accessor = accessor_class.unwrap_or("");
    let cache_key = format!(
        "completion:{}:{}:{}:{}:{}:{}",
        class_name,
        prefix,
        accessor,
        assume_subclass_access as u8,
        include_impl_members as u8,
        declaration_context as u8,
    );

    if let Some(items) = read_completion_cache(&cache_key, &memory_cache, &persistent_cache)? {
        return Ok(items);
    }

    let mut class_ids = ctx.class_ids_by_name(&class_name)?;

    if class_ids.is_empty() {
        if let Some(base) = class_name.split('<').next() {
            class_ids = ctx.class_ids_by_name(base)?;
        }
    }

    let mut queue = VecDeque::from(
        class_ids
            .into_iter()
            .map(|class_id| (class_id, 0usize))
            .collect::<Vec<_>>(),
    );
    let mut visited_ids = HashSet::new();
    let mut visited_names = HashSet::new();
    let mut seen_items = HashSet::new();
    let mut items = Vec::new();

    while let Some((class_id, class_rank)) = queue.pop_front() {
        if !visited_ids.insert(class_id) {
            continue;
        }

        let current_class_name = class_name_by_id(ctx.conn, class_id).unwrap_or_default();
        visited_names.insert(current_class_name.clone());

        append_members_for_class(
            ctx,
            class_id,
            &current_class_name,
            class_rank,
            &prefix,
            accessor,
            assume_subclass_access,
            include_impl_members,
            declaration_context,
            &mut seen_items,
            &mut items,
        )?;

        append_enum_items(
            ctx.conn,
            class_id,
            class_rank,
            &prefix,
            &mut seen_items,
            &mut items,
        )?;

        for (parent_id, parent_name) in parent_classes(ctx.conn, class_id)? {
            if !parent_name.is_empty() && !visited_names.insert(parent_name.clone()) {
                continue;
            }

            if let Some(parent_id) = parent_id {
                queue.push_back((parent_id, class_rank + 1));
            }

            for id in ctx.class_ids_by_name(&parent_name)? {
                queue.push_back((id, class_rank + 1));
            }
        }

        if items.len() >= MAX_COMPLETION_ITEMS {
            break;
        }
    }

    write_completion_cache(&cache_key, &items, &memory_cache, &persistent_cache);

    Ok(items)
}

/// Append members from one class.
/// 添加某个 class 的成员补全。
fn append_members_for_class(
    ctx: &mut CompletionContext,
    class_id: i64,
    owner_class: &str,
    class_rank: usize,
    prefix: &str,
    accessor_class: &str,
    assume_subclass_access: bool,
    include_impl_members: bool,
    declaration_context: bool,
    seen: &mut HashSet<String>,
    items: &mut Vec<Value>,
) -> Result<()> {
    let mut sql = String::from(
        r#"
        SELECT
            smn.text,
            smt.text,
            srt.text,
            m.access,
            m.detail,
            m.line_number,
            dp.full_path || '/' || sn.text
        FROM members m
        JOIN strings smn ON m.name_id = smn.id
        JOIN strings smt ON m.type_id = smt.id
        LEFT JOIN strings srt ON m.return_type_id = srt.id
        LEFT JOIN files f ON COALESCE(m.file_id, NULL) = f.id
        LEFT JOIN dir_paths dp ON f.directory_id = dp.id
        LEFT JOIN strings sn ON f.filename_id = sn.id
        WHERE m.class_id = ?
        "#,
    );

    if !include_impl_members {
        sql.push_str(" AND (m.access IS NULL OR m.access != 'impl')");
    }

    sql.push_str(" ORDER BY smn.text");

    let mut stmt = ctx.conn.prepare(&sql)?;

    let mut rows = stmt.query(params![class_id])?;

    let mut matched = Vec::new();

    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        let member_type: String = row.get(1)?;
        let return_type: Option<String> = row.get(2)?;
        let access: Option<String> = row.get(3)?;
        let detail: Option<String> = row.get(4)?;
        let line: Option<usize> = row.get::<_, Option<i64>>(5)?.map(|v| v as usize);
        let file_path: Option<String> = row.get(6).ok().flatten();
        let match_rank = completion_match_rank(&name, prefix);

        if match_rank == COMPLETION_MATCH_NONE {
            continue;
        }

        if !is_member_accessible(
            ctx,
            owner_class,
            accessor_class,
            access.as_deref(),
            assume_subclass_access,
        )? {
            continue;
        }

        let signature_info = if member_type == "function" {
            resolve_function_signature_info(
                detail.as_deref(),
                file_path.as_deref(),
                line,
                &name,
                &mut ctx.file_cache,
            )
        } else {
            None
        };
        let function_detail = signature_info
            .as_ref()
            .map(|info| info.detail.as_str())
            .or(detail.as_deref());

        let detail_text = if member_type == "function" {
            function_completion_detail(return_type.as_deref(), function_detail, Some(owner_class))
        } else {
            member_detail(return_type.as_deref(), owner_class)
        };
        let dedupe_key = format!("{}:{}", name, detail_text);

        if !seen.insert(dedupe_key) {
            continue;
        }

        let documentation = file_path
            .as_deref()
            .and_then(|path| line.map(|line| extract_comment_from_file(path, line, &mut ctx.file_cache)))
            .unwrap_or_default();

        let documentation = merge_docs(
            documentation,
            function_detail
                .map(|text| text.to_string())
                .or(detail.clone()),
        );
        let kind = completion_kind(&member_type);
        let sort_text = completion_sort_text(class_rank * 1000 + match_rank, kind, &name);
        let insert_text = if member_type == "function" {
            if declaration_context {
                let should_override = owner_class != accessor_class
                    && signature_info
                        .as_ref()
                        .map(|info| !info.is_static)
                        .unwrap_or(true);
                function_declaration_insert_text(
                    return_type.as_deref(),
                    &name,
                    function_detail,
                    should_override,
                )
            } else {
                function_snippet_text(&name, function_detail)
            }
        } else {
            name.clone()
        };
        let insert_text_format = if member_type == "function" && !declaration_context {
            2
        } else {
            1
        };

        matched.push(json!({
            "label": name,
            "kind": kind,
            "detail": detail_text,
            "documentation": documentation,
            "insertText": insert_text,
            "insertTextFormat": insert_text_format,
            "filterText": name,
            "sortText": sort_text,
            "score_offset": completion_score_offset(match_rank),
            "labelDetails": {
                "detail": format!(" {}", detail_text),
                "description": owner_class,
            },
            "sourceClass": owner_class,
        }));
    }

    matched.sort_by(|left, right| {
        let left_sort = left
            .get("sortText")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let right_sort = right
            .get("sortText")
            .and_then(Value::as_str)
            .unwrap_or_default();

        left_sort
            .cmp(right_sort)
            .then_with(|| {
                left.get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .cmp(
                        right
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
            })
    });

    for item in matched.into_iter().take(MAX_MEMBER_ITEMS_PER_CLASS) {
        items.push(item);
    }

    Ok(())
}

/// Build a compact member detail string for completion menus.
/// 构造补全菜单里紧凑的成员说明文本。
fn member_detail(return_type: Option<&str>, owner_class: &str) -> String {
    let return_type = return_type
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .unwrap_or("member");

    if owner_class.is_empty() {
        return return_type.to_string();
    }

    format!("{} - {}", return_type, owner_class)
}

/// Check C++ access visibility.
/// 检查 C++ public/protected/private 可见性。
fn is_member_accessible(
    ctx: &mut CompletionContext,
    owner_class: &str,
    accessor_class: &str,
    access: Option<&str>,
    assume_subclass_access: bool,
) -> Result<bool> {
    let access = access.unwrap_or("");

    if accessor_class.is_empty() {
        return Ok(access.is_empty() || access == "public");
    }

    if accessor_class == owner_class {
        return Ok(true);
    }

    if access == "private" {
        return Ok(false);
    }

    if access == "protected" {
        if assume_subclass_access {
            return Ok(true);
        }
        return is_subclass_of(ctx, accessor_class, owner_class);
    }

    Ok(true)
}

/// Append enum values.
/// 添加 enum item 补全。
fn append_enum_items(
    conn: &Connection,
    class_id: i64,
    class_rank: usize,
    prefix: &str,
    seen: &mut HashSet<String>,
    items: &mut Vec<Value>,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT sen.text FROM enum_values ev JOIN strings sen ON ev.name_id = sen.id WHERE ev.enum_id = ?",
    );

    sql.push_str(" ORDER BY sen.text");

    let mut stmt = conn.prepare(&sql)?;

    let mut rows = stmt.query(params![class_id])?;

    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        let match_rank = completion_match_rank(&name, prefix);

        if match_rank == COMPLETION_MATCH_NONE {
            continue;
        }

        if seen.insert(format!("enum:{}", name)) {
            let kind = 20;
            items.push(json!({
                "label": name,
                "kind": kind,
                "detail": "enum item",
                "filterText": name,
                "sortText": completion_sort_text(class_rank * 1000 + match_rank, kind, &name),
                "insertText": name,
                "score_offset": completion_score_offset(match_rank),
            }));
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// DB helpers
// -----------------------------------------------------------------------------

/// Find member return type through inheritance.
/// 沿继承链查找成员返回类型。
fn find_member_return_type(
    ctx: &mut CompletionContext,
    class_name: &str,
    member_name: &str,
) -> Result<Option<String>> {
    let class_name = resolve_typedef(ctx, class_name)?;
    let mut queue = VecDeque::from(ctx.class_ids_by_name(&class_name)?);
    let mut visited = HashSet::new();

    while let Some(class_id) = queue.pop_front() {
        if !visited.insert(class_id) {
            continue;
        }

        let result = ctx
            .conn
            .query_row(
                r#"
                SELECT srt.text
                FROM members m
                JOIN strings sm ON m.name_id = sm.id
                LEFT JOIN strings srt ON m.return_type_id = srt.id
                WHERE m.class_id = ?
                  AND sm.text = ?
                ORDER BY
                    CASE WHEN srt.text IS NULL OR srt.text = 'void' THEN 1 ELSE 0 END,
                    length(srt.text) DESC
                LIMIT 1
                "#,
                params![class_id, member_name],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();

        if let Some(result) = result {
            return Ok(Some(clean_type(&result)));
        }

        for (parent_id, parent_name) in parent_classes(ctx.conn, class_id)? {
            if let Some(parent_id) = parent_id {
                queue.push_back(parent_id);
            }

            for id in ctx.class_ids_by_name(&parent_name)? {
                queue.push_back(id);
            }
        }
    }

    Ok(None)
}

/// Get parent classes from inheritance table.
/// 从 inheritance 表获取父类。
fn parent_classes(conn: &Connection, class_id: i64) -> Result<Vec<(Option<i64>, String)>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT i.parent_class_id, si.text
        FROM inheritance i
        JOIN strings si ON i.parent_name_id = si.id
        WHERE i.child_id = ?
        "#,
    )?;

    let rows = stmt.query_map([class_id], |row| {
        Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, String>(1)?))
    })?;

    Ok(rows.filter_map(|row| row.ok()).collect())
}

/// Get class name from class id.
/// 根据 class id 获取 class name。
fn class_name_by_id(conn: &Connection, class_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT s.text FROM classes c JOIN strings s ON c.name_id = s.id WHERE c.id = ?",
        [class_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Return true if child class derives from parent class.
/// 判断 child 是否继承自 parent。
fn is_subclass_of(ctx: &mut CompletionContext, child: &str, parent: &str) -> Result<bool> {
    if child == parent {
        return Ok(true);
    }

    let key = (child.to_string(), parent.to_string());

    if let Some(value) = ctx.inheritance_cache.get(&key) {
        return Ok(*value);
    }

    let parent_ids = ctx.class_ids_by_name(parent)?;
    let mut queue = VecDeque::from(ctx.class_ids_by_name(child)?);
    let mut visited = HashSet::new();

    while let Some(class_id) = queue.pop_front() {
        if parent_ids.contains(&class_id) {
            ctx.inheritance_cache.insert(key, true);
            return Ok(true);
        }

        if !visited.insert(class_id) {
            continue;
        }

        for (parent_id, parent_name) in parent_classes(ctx.conn, class_id)? {
            if parent_name == parent {
                ctx.inheritance_cache.insert(key, true);
                return Ok(true);
            }

            if let Some(parent_id) = parent_id {
                queue.push_back(parent_id);
            }

            for id in ctx.class_ids_by_name(&parent_name)? {
                queue.push_back(id);
            }
        }
    }

    ctx.inheritance_cache.insert(key, false);
    Ok(false)
}

/// Resolve simple typedef aliases.
/// 解析简单 typedef 别名。
fn resolve_typedef(ctx: &mut CompletionContext, type_name: &str) -> Result<String> {
    let mut current = clean_type(type_name);

    for _ in 0..MAX_TYPEDEF_DEPTH {
        if current.is_empty() || current == "void" || current == "T" {
            break;
        }

        let Some(name_id) = ctx.string_id(&current)? else {
            break;
        };

        let next = ctx
            .conn
            .query_row(
                r#"
                SELECT sb.text
                FROM classes c
                JOIN strings sb ON c.base_class_id = sb.id
                WHERE c.name_id = ?
                  AND c.symbol_type = 'typedef'
                LIMIT 1
                "#,
                [name_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();

        let Some(next) = next.map(|s| clean_type(&s)) else {
            break;
        };

        if next.is_empty() || next == current {
            break;
        }

        current = next;
    }

    Ok(current)
}

/// Check whether a type exists in DB.
/// 检查类型是否存在于 DB。
fn is_known_type(ctx: &mut CompletionContext, name: &str) -> Result<bool> {
    let name = clean_type(name);

    let Some(name_id) = ctx.string_id(&name)? else {
        return Ok(false);
    };

    let exists = ctx
        .conn
        .prepare("SELECT 1 FROM classes WHERE name_id = ? LIMIT 1")?
        .exists([name_id])?;

    Ok(exists)
}

// -----------------------------------------------------------------------------
// Global symbols, snippets, macro specifiers
// -----------------------------------------------------------------------------

/// Fetch global class/struct/enum completions.
/// 获取全局 class/struct/enum 补全。
fn fetch_global_symbols(conn: &Connection, prefix: &str) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    let mut seen = HashSet::new();

    append_global_type_items(conn, prefix, &mut seen, &mut items)?;
    append_global_enum_value_items(conn, prefix, &mut seen, &mut items)?;

    Ok(items)
}

fn global_search_patterns(prefix: &str) -> Vec<(String, usize)> {
    let prefix_lower = prefix.trim().to_ascii_lowercase();
    if prefix_lower.is_empty() {
        return Vec::new();
    }

    let prefix_words = split_identifier_words(prefix);
    let mut patterns = vec![(format!("{}%", prefix_lower), 80usize)];

    let weak_pattern = if prefix_words.len() >= 2 {
        prefix_words
            .first()
            .map(|word| format!("{}%", word))
            .unwrap_or_else(|| format!("{}%", prefix_lower))
    } else {
        format!("%{}%", prefix_lower)
    };

    if weak_pattern != patterns[0].0 {
        patterns.push((weak_pattern, 200));
    }

    patterns
}

fn append_global_type_items(
    conn: &Connection,
    prefix: &str,
    seen: &mut HashSet<String>,
    items: &mut Vec<Value>,
) -> Result<()> {
    for (pattern, limit) in global_search_patterns(prefix) {
        let mut stmt = conn.prepare(
            r#"
            SELECT s.text, c.symbol_type
            FROM classes c
            JOIN strings s ON c.name_id = s.id
            WHERE c.symbol_type IN ('class', 'struct', 'UCLASS', 'USTRUCT', 'enum', 'UENUM', 'typedef')
              AND lower(s.text) LIKE ?
            ORDER BY s.text
            LIMIT ?
            "#,
        )?;

        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            let name: String = row.get(0)?;
            let symbol_type: String = row.get(1)?;
            Ok((name, symbol_type))
        })?;

        for row in rows.flatten() {
            let (name, symbol_type) = row;
            let match_rank = completion_match_rank(&name, prefix);
            if match_rank == COMPLETION_MATCH_NONE || !seen.insert(name.clone()) {
                continue;
            }

            let kind = if matches!(symbol_type.as_str(), "enum" | "UENUM") {
                13
            } else {
                7
            };

            items.push(json!({
                "label": name,
                "kind": kind,
                "detail": symbol_type,
                "filterText": name,
                "sortText": completion_sort_text(100_000 + match_rank, kind, &name),
                "insertText": name,
                "score_offset": completion_score_offset(match_rank),
                "labelDetails": {
                    "detail": format!(" {}", symbol_type),
                    "description": "UCore",
                },
            }));

            if items.len() >= 80 {
                return Ok(());
            }
        }
    }

    Ok(())
}

fn append_global_enum_value_items(
    conn: &Connection,
    prefix: &str,
    seen: &mut HashSet<String>,
    items: &mut Vec<Value>,
) -> Result<()> {
    if items.len() >= 80 {
        return Ok(());
    }

    for (pattern, limit) in global_search_patterns(prefix) {
        let mut stmt = conn.prepare(
            r#"
            SELECT sen.text, sclass.text
            FROM enum_values ev
            JOIN strings sen ON ev.name_id = sen.id
            JOIN classes c ON ev.enum_id = c.id
            JOIN strings sclass ON c.name_id = sclass.id
            WHERE lower(sen.text) LIKE ?
            ORDER BY sen.text
            LIMIT ?
            "#,
        )?;

        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            let name: String = row.get(0)?;
            let owner: String = row.get(1)?;
            Ok((name, owner))
        })?;

        for row in rows.flatten() {
            let (name, owner) = row;
            let match_rank = completion_match_rank(&name, prefix);
            if match_rank == COMPLETION_MATCH_NONE || !seen.insert(name.clone()) {
                continue;
            }

            items.push(json!({
                "label": name,
                "kind": 20,
                "detail": format!("enum item - {}", owner),
                "filterText": name,
                "sortText": completion_sort_text(100_500 + match_rank, 20, &name),
                "insertText": name,
                "score_offset": completion_score_offset(match_rank),
            }));

            if items.len() >= 80 {
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Complete indexed headers inside an unfinished #include string.
/// 在未完成的 #include 字符串内补全已索引头文件。
fn complete_include_paths(
    ctx: &mut CompletionContext,
    engine_ctx: Option<&mut CompletionContext>,
    content: &str,
    line: u32,
    character: u32,
) -> Result<Option<Vec<Value>>> {
    let Some(offset) = byte_offset_at(content, line as usize, character as usize) else {
        return Ok(None);
    };

    let line_start = content[..offset].rfind('\n').map(|index| index + 1).unwrap_or(0);
    let before = &content[line_start..offset];
    let trimmed = before.trim_start();

    if !trimmed.starts_with("#include") {
        return Ok(None);
    }

    let Some(prefix) = include_prefix(before) else {
        return Ok(None);
    };

    let mut items = fetch_include_paths(ctx, &prefix, line, character)?;

    if let Some(engine_ctx) = engine_ctx {
        let engine_items = fetch_include_paths(engine_ctx, &prefix, line, character)?;
        merge_completion_items(&mut items, engine_items, MAX_COMPLETION_ITEMS);
    }

    Ok(Some(items))
}

fn include_prefix(before_cursor: &str) -> Option<String> {
    let quote = before_cursor.rfind('"');
    let angle = before_cursor.rfind('<');
    let start = match (quote, angle) {
        (Some(q), Some(a)) => q.max(a),
        (Some(q), None) => q,
        (None, Some(a)) => a,
        (None, None) => return None,
    };

    let prefix = &before_cursor[start + 1..];

    if prefix.contains('"') || prefix.contains('>') {
        return None;
    }

    Some(prefix.trim_start().replace('\\', "/"))
}

fn fetch_include_paths(
    ctx: &mut CompletionContext,
    prefix: &str,
    line: u32,
    character: u32,
) -> Result<Vec<Value>> {
    let pattern = format!("{}%", prefix);
    let basename_pattern = format!("%/{}%", prefix);
    let prefix_start = character.saturating_sub(prefix.len() as u32);
    let mut stmt = ctx.conn.prepare(
        r#"
        SELECT
            CASE
                WHEN dp.full_path = '/' THEN '/' || sn.text
                WHEN substr(dp.full_path, -1) = '/' THEN dp.full_path || sn.text
                ELSE dp.full_path || '/' || sn.text
            END AS path
        FROM files f
        JOIN strings sn ON f.filename_id = sn.id
        JOIN dir_paths dp ON f.directory_id = dp.id
        WHERE f.is_header = 1
          AND (path LIKE ? OR path LIKE ?)
        ORDER BY
          CASE
            WHEN path LIKE '%/Public/%' THEN 0
            WHEN path LIKE '%/Classes/%' THEN 1
            WHEN path LIKE '%/Private/%' THEN 2
            ELSE 3
          END,
          length(path),
          path
        LIMIT 80
        "#,
    )?;

    let rows = stmt.query_map(params![pattern, basename_pattern], |row| row.get::<_, String>(0))?;

    let mut items = Vec::new();
    let mut seen = HashSet::new();

    for row in rows.flatten() {
        let path = row;
        let include_path = include_insert_path(&path);
        let Some((candidate, kind, sort_bucket)) = include_candidate(&include_path, prefix) else {
            continue;
        };

        if !seen.insert(candidate.clone()) {
            continue;
        }

        items.push(json!({
            "label": candidate,
            "kind": kind,
            "detail": if kind == 19 { "include dir" } else { "include" },
            "documentation": path,
            "filterText": candidate,
            "insertText": candidate,
            "sortText": completion_sort_text(sort_bucket, kind, &include_path),
            "textEdit": {
                "newText": candidate,
                "range": {
                    "start": {
                        "line": line,
                        "character": prefix_start,
                    },
                    "end": {
                        "line": line,
                        "character": character,
                    },
                },
            },
        }));
    }

    Ok(items)
}

fn include_insert_path(path: &str) -> String {
    for marker in ["/Public/", "/Classes/", "/Private/"] {
        if let Some(index) = path.find(marker) {
            return path[index + marker.len()..].to_string();
        }
    }

    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string()
}

fn include_candidate(include_path: &str, prefix: &str) -> Option<(String, i64, usize)> {
    let include_lower = include_path.to_ascii_lowercase();
    let prefix_lower = prefix.to_ascii_lowercase();

    if prefix.is_empty() {
        if let Some((head, _)) = include_path.split_once('/') {
            return Some((format!("{}/", head), 19, 5));
        }

        return Some((include_path.to_string(), 17, 10));
    }

    if include_lower.starts_with(&prefix_lower) {
        let remainder = &include_path[prefix.len()..];

        if remainder.is_empty() {
            return Some((include_path.to_string(), 17, 10));
        }

        if let Some(rest) = remainder.strip_prefix('/') {
            let next = rest.split('/').next().unwrap_or(rest);
            if rest.contains('/') {
                return Some((format!("{}/{}/", prefix, next), 19, 5));
            }

            return Some((format!("{}/{}", prefix, next), 17, 10));
        }

        let next = remainder.split('/').next().unwrap_or(remainder);
        if remainder.contains('/') {
            return Some((format!("{}{}/", prefix, next), 19, 5));
        }

        return Some((format!("{}{}", prefix, next), 17, 10));
    }

    if !prefix.contains('/') {
        let filename = include_path.rsplit('/').next().unwrap_or(include_path);
        if filename.to_ascii_lowercase().starts_with(&prefix_lower) {
            return Some((include_path.to_string(), 17, 20));
        }
    }

    None
}

fn complete_preprocessor_directives(content: &str, line: u32, character: u32) -> Option<Value> {
    let offset = byte_offset_at(content, line as usize, character as usize)?;
    let line_start = content[..offset].rfind('\n').map(|index| index + 1).unwrap_or(0);
    let before = &content[line_start..offset];
    let trimmed = before.trim_start();

    if !trimmed.starts_with('#') {
        return None;
    }

    if trimmed.contains('"') || trimmed.contains('<') || trimmed.contains('>') {
        return None;
    }

    let after_hash = trimmed[1..].trim_start();
    if after_hash.contains(' ') || after_hash.contains('\t') {
        return None;
    }

    let prefix = after_hash;
    let start_in_trimmed = trimmed.len().saturating_sub(prefix.len());
    let start_in_before = before.len().saturating_sub(trimmed.len()) + start_in_trimmed;
    let start_char = before[..start_in_before].chars().count() as u32;

    let directives = preprocessor_directive_items(prefix, line, start_char, character);
    if directives.is_empty() {
        None
    } else {
        Some(json!(directives))
    }
}

fn preprocessor_directive_items(
    prefix: &str,
    line: u32,
    start_char: u32,
    end_char: u32,
) -> Vec<Value> {
    let candidates = [
        ("include", "include \"$1\"", "preprocessor include"),
        ("pragma once", "pragma once", "header guard pragma"),
        ("define", "define $1", "preprocessor define"),
        ("ifdef", "ifdef $1", "preprocessor ifdef"),
        ("ifndef", "ifndef $1", "preprocessor ifndef"),
        ("if", "if $1", "preprocessor if"),
        ("elif", "elif $1", "preprocessor elif"),
        ("else", "else", "preprocessor else"),
        ("endif", "endif", "preprocessor endif"),
        ("undef", "undef $1", "preprocessor undef"),
    ];

    let prefix_lower = prefix.to_ascii_lowercase();
    let mut items = Vec::new();

    for (label, insert_text, detail) in candidates {
        if !label.starts_with(&prefix_lower) {
            continue;
        }

        items.push(json!({
            "label": label,
            "kind": 14,
            "detail": detail,
            "filterText": label,
            "insertText": insert_text,
            "insertTextFormat": 2,
            "sortText": completion_sort_text(40, 14, label),
            "textEdit": {
                "newText": insert_text,
                "range": {
                    "start": {
                        "line": line,
                        "character": start_char,
                    },
                    "end": {
                        "line": line,
                        "character": end_char,
                    },
                },
            },
        }));
    }

    items
}

/// Complete Unreal macro specifiers.
/// 补全 Unreal 宏参数 specifier。
fn complete_macro_specifiers(node: Node, content: &str) -> Option<Value> {
    let mut current = Some(node);

    while let Some(n) = current {
        if matches!(n.kind(), "unreal_macro_argument_list" | "macro_argument_list") {
            let parent = n.parent()?;
            let text = node_text(parent, content);
            let macro_name = text.split('(').next()?.trim();
            let prefix = completion_prefix(node, content);
            return macro_specifiers(macro_name, &prefix, false);
        }

        current = n.parent();
    }

    None
}

/// Complete macro specifiers while the macro argument list is syntactically incomplete.
/// 当宏参数列表还没形成完整语法节点时，用光标前文本兜底补全 specifier。
fn complete_macro_specifiers_at(content: &str, line: u32, character: u32) -> Option<Value> {
    let offset = byte_offset_at(content, line as usize, character as usize)?;
    let before = &content[..offset];
    let (macro_name, macro_open) = unreal_macro_call_before_cursor(before)?;
    let after_open = &before[macro_open + 1..];

    if after_open.contains(')') || after_open.contains(';') || after_open.contains('{') {
        return None;
    }

    let prefix = before
        .rsplit(['(', ',', '='])
        .next()
        .unwrap_or("")
        .rsplit(|ch: char| !matches!(ch, '_' | 'A'..='Z' | 'a'..='z' | '0'..='9'))
        .next()
        .unwrap_or("");

    macro_specifiers(macro_name, prefix, is_in_meta_argument(after_open))
}

fn unreal_macro_call_before_cursor(before: &str) -> Option<(&'static str, usize)> {
    [
        "UPROPERTY",
        "UFUNCTION",
        "UCLASS",
        "USTRUCT",
        "UENUM",
        "UPARAM",
        "UMETA",
    ]
    .iter()
    .filter_map(|name| {
        let pattern = format!("{}(", name);
        before.rfind(&pattern).map(|index| (*name, index + name.len()))
    })
    .max_by_key(|(_, open)| *open)
}

/// Return macro specifier items.
/// 返回宏参数补全项。
fn macro_specifiers(macro_name: &str, prefix: &str, in_meta: bool) -> Option<Value> {
    let mut specs = if in_meta {
        meta_specifier_specs(macro_name)?
    } else {
        macro_specifier_specs(macro_name)?
    };

    if !prefix.is_empty() {
        let lower = prefix.to_ascii_lowercase();
        specs.retain(|spec| spec.label.to_ascii_lowercase().starts_with(&lower));
    }

    Some(json!(
        specs
            .into_iter()
            .map(|spec| {
                json!({
                    "label": spec.label,
                    "kind": 12,
                    "detail": spec.detail,
                    "insertText": spec.insert_text,
                    "insertTextFormat": spec.insert_text_format,
                    "filterText": spec.label,
                    "sortText": completion_sort_text(80, 12, spec.label),
                    "score_offset": 16,
                })
            })
            .collect::<Vec<_>>()
    ))
}

/// Unreal snippets and common helpers.
/// Unreal 常用 snippet 和 helper 补全。
fn should_offer_ue_snippets(prefix: &str) -> bool {
    let prefix = prefix.trim();

    if prefix.chars().count() < 2 {
        return false;
    }

    let lower = prefix.to_ascii_lowercase();
    let starts_upper = prefix
        .chars()
        .next()
        .map(|ch| ch.is_ascii_uppercase())
        .unwrap_or(false);

    if starts_upper && matches!(lower.chars().next(), Some('u' | 't' | 'f')) {
        return true;
    }

    if lower.starts_with("ue") {
        return true;
    }

    if prefix.chars().count() < 3 {
        return false;
    }

    [
        "cas", "cre", "def", "gen", "imp", "loa", "mak", "new", "nsl", "sta", "sup", "tex",
    ]
    .iter()
    .any(|known| lower.starts_with(known))
}

fn is_in_meta_argument(text_after_open: &str) -> bool {
    let lower = text_after_open.to_ascii_lowercase();
    let Some(meta_index) = lower.rfind("meta") else {
        return false;
    };

    let after_meta = lower[meta_index + "meta".len()..].trim_start();
    after_meta.starts_with('=') || after_meta.starts_with('(')
}

#[derive(Clone, Copy)]
struct MacroSpecifierSpec {
    label: &'static str,
    detail: &'static str,
    insert_text: &'static str,
    insert_text_format: i64,
}

fn macro_plain(label: &'static str, detail: &'static str) -> MacroSpecifierSpec {
    MacroSpecifierSpec {
        label,
        detail,
        insert_text: label,
        insert_text_format: 1,
    }
}

fn macro_snippet(
    label: &'static str,
    detail: &'static str,
    insert_text: &'static str,
) -> MacroSpecifierSpec {
    MacroSpecifierSpec {
        label,
        detail,
        insert_text,
        insert_text_format: 2,
    }
}

fn macro_specifier_specs(macro_name: &str) -> Option<Vec<MacroSpecifierSpec>> {
    Some(match macro_name {
        "UPROPERTY" => vec![
            macro_plain("EditAnywhere", "property specifier"),
            macro_plain("EditDefaultsOnly", "property specifier"),
            macro_plain("EditInstanceOnly", "property specifier"),
            macro_plain("VisibleAnywhere", "property specifier"),
            macro_plain("VisibleDefaultsOnly", "property specifier"),
            macro_plain("VisibleInstanceOnly", "property specifier"),
            macro_plain("BlueprintReadOnly", "property specifier"),
            macro_plain("BlueprintReadWrite", "property specifier"),
            macro_plain("BlueprintAssignable", "property specifier"),
            macro_plain("BlueprintCallable", "property specifier"),
            macro_plain("Config", "property specifier"),
            macro_plain("GlobalConfig", "property specifier"),
            macro_plain("Transient", "property specifier"),
            macro_plain("DuplicateTransient", "property specifier"),
            macro_plain("SaveGame", "property specifier"),
            macro_plain("Instanced", "property specifier"),
            macro_plain("Replicated", "property specifier"),
            macro_snippet(
                "ReplicatedUsing",
                "property specifier",
                "ReplicatedUsing=${1:OnRep_Function}",
            ),
            macro_snippet("Category", "property key", "Category=\"${1:Default}\""),
            macro_snippet("meta", "metadata key", "meta=(${1})"),
        ],

        "UFUNCTION" => vec![
            macro_plain("BlueprintCallable", "function specifier"),
            macro_plain("BlueprintPure", "function specifier"),
            macro_plain("BlueprintImplementableEvent", "function specifier"),
            macro_plain("BlueprintNativeEvent", "function specifier"),
            macro_plain("BlueprintAuthorityOnly", "function specifier"),
            macro_plain("BlueprintCosmetic", "function specifier"),
            macro_plain("CallInEditor", "function specifier"),
            macro_plain("Client", "network specifier"),
            macro_plain("Server", "network specifier"),
            macro_plain("NetMulticast", "network specifier"),
            macro_plain("Reliable", "network specifier"),
            macro_plain("Unreliable", "network specifier"),
            macro_plain("WithValidation", "network specifier"),
            macro_plain("Exec", "function specifier"),
            macro_snippet("Category", "function key", "Category=\"${1:Default}\""),
            macro_snippet("meta", "metadata key", "meta=(${1})"),
        ],

        "UCLASS" => vec![
            macro_plain("Blueprintable", "type specifier"),
            macro_plain("BlueprintType", "type specifier"),
            macro_plain("Abstract", "type specifier"),
            macro_plain("NotBlueprintable", "type specifier"),
            macro_plain("Config", "type specifier"),
            macro_plain("DefaultConfig", "type specifier"),
            macro_plain("EditInlineNew", "type specifier"),
            macro_plain("CollapseCategories", "type specifier"),
            macro_snippet("HideCategories", "type key", "HideCategories=\"${1:Category}\""),
            macro_snippet("ShowCategories", "type key", "ShowCategories=\"${1:Category}\""),
            macro_snippet("ClassGroup", "type key", "ClassGroup=\"${1:Group}\""),
            macro_snippet("meta", "metadata key", "meta=(${1})"),
        ],

        "USTRUCT" => vec![
            macro_plain("BlueprintType", "type specifier"),
            macro_plain("Atomic", "type specifier"),
            macro_plain("NoExport", "type specifier"),
            macro_snippet("meta", "metadata key", "meta=(${1})"),
        ],

        "UENUM" => vec![
            macro_plain("BlueprintType", "enum specifier"),
            macro_snippet("ScriptName", "enum key", "ScriptName=\"${1:Name}\""),
            macro_snippet("meta", "metadata key", "meta=(${1})"),
        ],

        "UPARAM" | "UMETA" => meta_specifier_specs(macro_name)?,

        _ => return None,
    })
}

fn meta_specifier_specs(macro_name: &str) -> Option<Vec<MacroSpecifierSpec>> {
    let common = vec![
        macro_snippet("DisplayName", "metadata key", "DisplayName=\"${1:Name}\""),
        macro_snippet("ToolTip", "metadata key", "ToolTip=\"${1:Description}\""),
        macro_snippet("ShortToolTip", "metadata key", "ShortToolTip=\"${1:Description}\""),
        macro_plain("DeprecatedFunction", "metadata key"),
        macro_snippet(
            "DeprecationMessage",
            "metadata key",
            "DeprecationMessage=\"${1:Message}\"",
        ),
        macro_plain("DevelopmentOnly", "metadata key"),
        macro_snippet("ScriptName", "metadata key", "ScriptName=\"${1:Name}\""),
    ];

    let labels = match macro_name {
        "UPROPERTY" => vec![
            macro_snippet(
                "AllowPrivateAccess",
                "metadata key",
                "AllowPrivateAccess=\"${1:true}\"",
            ),
            macro_snippet("ClampMin", "metadata key", "ClampMin=\"${1:0}\""),
            macro_snippet("ClampMax", "metadata key", "ClampMax=\"${1:100}\""),
            macro_snippet("UIMin", "metadata key", "UIMin=\"${1:0}\""),
            macro_snippet("UIMax", "metadata key", "UIMax=\"${1:100}\""),
            macro_snippet("Units", "metadata key", "Units=\"${1:cm}\""),
            macro_snippet("EditCondition", "metadata key", "EditCondition=\"${1:bEnabled}\""),
            macro_plain("EditConditionHides", "metadata key"),
            macro_plain("BindWidget", "metadata key"),
            macro_plain("BindWidgetOptional", "metadata key"),
            macro_plain("ExposeOnSpawn", "metadata key"),
            macro_plain("MakeEditWidget", "metadata key"),
            macro_plain("MultiLine", "metadata key"),
            macro_snippet("AllowedClasses", "metadata key", "AllowedClasses=\"${1:Class}\""),
            macro_snippet("DisallowedClasses", "metadata key", "DisallowedClasses=\"${1:Class}\""),
        ],

        "UFUNCTION" => vec![
            macro_snippet("WorldContext", "metadata key", "WorldContext=\"${1:WorldContextObject}\""),
            macro_plain("CallableWithoutWorldContext", "metadata key"),
            macro_snippet("DefaultToSelf", "metadata key", "DefaultToSelf=\"${1:Target}\""),
            macro_snippet("HidePin", "metadata key", "HidePin=\"${1:PinName}\""),
            macro_plain("AdvancedDisplay", "metadata key"),
            macro_snippet("AutoCreateRefTerm", "metadata key", "AutoCreateRefTerm=\"${1:Param}\""),
            macro_snippet("DeterminesOutputType", "metadata key", "DeterminesOutputType=\"${1:ClassParam}\""),
            macro_snippet("ExpandEnumAsExecs", "metadata key", "ExpandEnumAsExecs=\"${1:EnumParam}\""),
            macro_plain("Latent", "metadata key"),
            macro_snippet("LatentInfo", "metadata key", "LatentInfo=\"${1:LatentInfo}\""),
            macro_snippet("CompactNodeTitle", "metadata key", "CompactNodeTitle=\"${1:Title}\""),
            macro_snippet("Keywords", "metadata key", "Keywords=\"${1:Keyword1 Keyword2}\""),
        ],

        "UCLASS" => vec![
            macro_plain("BlueprintSpawnableComponent", "metadata key"),
            macro_plain("ChildCanTick", "metadata key"),
            macro_plain("ChildCannotTick", "metadata key"),
            macro_plain("ShowWorldContextPin", "metadata key"),
            macro_plain("DontUseGenericSpawnObject", "metadata key"),
        ],

        "USTRUCT" => vec![
            macro_snippet("HasNativeMake", "metadata key", "HasNativeMake=\"${1:Module.Function}\""),
            macro_snippet("HasNativeBreak", "metadata key", "HasNativeBreak=\"${1:Module.Function}\""),
        ],
        "UENUM" | "UMETA" | "UPARAM" => Vec::new(),
        _ => return None,
    };

    Some(common.into_iter().chain(labels).collect())
}

/// Unreal snippets and common helpers.
/// Unreal 常用 snippet 和 helper 补全。
fn ue_snippets(prefix: &str) -> Vec<Value> {
    let mut items = vec![
        snippet("UCLASS", "UCLASS($1)", "Unreal class macro"),
        snippet("USTRUCT", "USTRUCT($1)", "Unreal struct macro"),
        snippet("UENUM", "UENUM($1)", "Unreal enum macro"),
        snippet("UDELEGATE", "UDELEGATE($1)", "Unreal delegate macro"),
        snippet("UINTERFACE", "UINTERFACE($1)", "Unreal interface macro"),
        snippet("UPROPERTY", "UPROPERTY($1)", "Unreal property macro"),
        snippet("UFUNCTION", "UFUNCTION($1)", "Unreal function macro"),
        snippet("UPARAM", "UPARAM($1)", "Unreal parameter macro"),
        snippet("UMETA", "UMETA($1)", "Unreal enum metadata macro"),
        snippet("GENERATED_BODY", "GENERATED_BODY()", "Unreal generated body"),
        snippet("GENERATED_UCLASS_BODY", "GENERATED_UCLASS_BODY()", "Legacy generated body"),
        snippet("GENERATED_UINTERFACE_BODY", "GENERATED_UINTERFACE_BODY()", "Legacy interface generated body"),
        snippet("DECLARE_LOG_CATEGORY_EXTERN", "DECLARE_LOG_CATEGORY_EXTERN($1, Log, All)", "Declare log category"),
        snippet("DEFINE_LOG_CATEGORY", "DEFINE_LOG_CATEGORY($1)", "Define log category"),
        snippet("UE_DEFINE_GAME_MODULE", "UE_DEFINE_GAME_MODULE($1, $2, $3)", "Define Unreal game module"),
        snippet("IMPLEMENT_PRIMARY_GAME_MODULE", "IMPLEMENT_PRIMARY_GAME_MODULE($1, $2, $3)", "Implement primary game module"),
        snippet("IMPLEMENT_MODULE", "IMPLEMENT_MODULE($1, $2)", "Implement Unreal module"),
        snippet("Super::", "Super::", "Parent class scope"),
        snippet("GetWorld()", "GetWorld()", "Get current world"),
        snippet("CreateDefaultSubobject", "CreateDefaultSubobject<$1>($2)", "Create default subobject"),
        snippet("NewObject", "NewObject<$1>($2)", "Create a new UObject"),
        snippet("DuplicateObject", "DuplicateObject<$1>($2, $3)", "Duplicate an object"),
        snippet("GetDefault", "GetDefault<$1>()", "Get class default object"),
        snippet("Cast", "Cast<$1>($2)", "Unreal safe cast"),
        snippet("CastChecked", "CastChecked<$1>($2)", "Checked Unreal cast"),
        snippet("CastField", "CastField<$1>($2)", "Unreal field cast"),
        snippet("StaticClass", "$1::StaticClass()", "Get Unreal UClass"),
        snippet("FindObject", "FindObject<$1>($2, $3)", "Find an existing UObject"),
        snippet("LoadObject", "LoadObject<$1>($2, $3)", "Load a UObject"),
        snippet("StaticLoadObject", "StaticLoadObject($1, $2, $3)", "Load a UObject by path"),
        snippet("LoadClass", "LoadClass<$1>($2, $3)", "Load a UClass"),
        snippet("StaticLoadClass", "StaticLoadClass($1, $2, $3)", "Load a UClass by path"),
        snippet("CreateWidget", "CreateWidget<$1>($2, $3)", "Create a widget"),
        snippet("TSubclassOf", "TSubclassOf<$1>", "Class reference wrapper"),
        snippet("TObjectPtr", "TObjectPtr<$1>", "Strong UObject pointer"),
        snippet("TWeakObjectPtr", "TWeakObjectPtr<$1>", "Weak UObject pointer"),
        snippet("TSoftObjectPtr", "TSoftObjectPtr<$1>", "Soft UObject pointer"),
        snippet("TSoftClassPtr", "TSoftClassPtr<$1>", "Soft class pointer"),
        snippet("TArray", "TArray<$1>", "Unreal dynamic array"),
        snippet("TMap", "TMap<$1, $2>", "Unreal map container"),
        snippet("TSet", "TSet<$1>", "Unreal set container"),
        snippet("TQueue", "TQueue<$1>", "Unreal queue container"),
        snippet("TOptional", "TOptional<$1>", "Optional value wrapper"),
        snippet("TArrayView", "TArrayView<$1>", "Array view wrapper"),
        snippet("TConstArrayView", "TConstArrayView<$1>", "Const array view wrapper"),
        snippet("TUniquePtr", "TUniquePtr<$1>", "Unique pointer"),
        snippet("TSharedPtr", "TSharedPtr<$1>", "Shared pointer"),
        snippet("TSharedRef", "TSharedRef<$1>", "Shared reference"),
        snippet("TWeakPtr", "TWeakPtr<$1>", "Weak shared pointer"),
        snippet("TScriptInterface", "TScriptInterface<$1>", "Script interface wrapper"),
        snippet("MakeShared", "MakeShared<$1>($2)", "Create a shared pointer"),
        snippet("MakeUnique", "MakeUnique<$1>($2)", "Create a unique pointer"),
        snippet("MakeShareable", "MakeShareable(new $1($2))", "Create a shared pointer from raw object"),
        snippet("MoveTemp", "MoveTemp($1)", "Move semantics helper"),
        snippet("Forward", "Forward<$1>($2)", "Perfect forwarding helper"),
        snippet("UE_LOG", "UE_LOG($1)", "Unreal logging macro"),
        snippet("UE_CLOG", "UE_CLOG($1)", "Conditional Unreal logging macro"),
        snippet("UE_LOGFMT", "UE_LOGFMT($1)", "Formatted Unreal logging macro"),
        snippet("check", "check($1)", "Debug assertion"),
        snippet("checkf", "checkf($1)", "Debug assertion with message"),
        snippet("ensure", "ensure($1)", "Runtime assertion"),
        snippet("ensureMsgf", "ensureMsgf($1)", "Runtime assertion with message"),
        snippet("ensureAlways", "ensureAlways($1)", "Always-on runtime assertion"),
        snippet("ensureAlwaysMsgf", "ensureAlwaysMsgf($1)", "Always-on runtime assertion with message"),
        snippet("verify", "verify($1)", "Assertion that keeps evaluating"),
        snippet("verifyf", "verifyf($1)", "Assertion that keeps evaluating with message"),
        snippet("TEXT", "TEXT($1)", "Wide text macro"),
        snippet("LOCTEXT", "LOCTEXT($1, $2)", "Localized text macro"),
        snippet("NSLOCTEXT", "NSLOCTEXT($1, $2, $3)", "Namespace localized text macro"),
        snippet("INVTEXT", "INVTEXT($1)", "Invariant text macro"),
        snippet("FString", "FString(TEXT($1))", "Unreal string type"),
        snippet("FName", "FName(TEXT($1))", "Unreal name type"),
        snippet("FText", "FText::FromString(TEXT($1))", "Localized text wrapper"),
        snippet("IsValid", "IsValid($1)", "Unreal validity helper"),
        snippet("IsValidLowLevel", "IsValidLowLevel($1)", "Low-level validity check"),
        snippet("GetName", "GetName()", "Get object name"),
        snippet("GetPathName", "GetPathName()", "Get object path name"),
        snippet("GetOuter", "GetOuter()", "Get outer object"),
        snippet("GetOwner", "GetOwner()", "Get actor owner"),
        snippet("GetActorLocation", "GetActorLocation()", "Get actor location"),
        snippet("GetActorRotation", "GetActorRotation()", "Get actor rotation"),
        snippet("GetComponentByClass", "GetComponentByClass<$1>()", "Get component by class"),
        snippet("AddDynamic", "AddDynamic(this, &$1::$2)", "Bind a dynamic delegate"),
        snippet("AddUObject", "AddUObject(this, &$1::$2)", "Bind a UObject delegate"),
        snippet("BindUObject", "BindUObject(this, &$1::$2)", "Bind a UObject delegate"),
        snippet("BindLambda", "BindLambda([&]($1) {\n\t$2\n})", "Bind a lambda"),
        snippet("Add", "Add($1)", "Add an item"),
        snippet("AddUnique", "AddUnique($1)", "Add an item if missing"),
        snippet("Emplace", "Emplace($1)", "Construct in place"),
        snippet("Reserve", "Reserve($1)", "Reserve container capacity"),
    ];

    if !prefix.is_empty() {
        let lower = prefix.to_ascii_lowercase();
        items.retain(|item| {
            item["label"]
                .as_str()
                .map(|label| label.to_ascii_lowercase().starts_with(&lower))
                .unwrap_or(false)
        });
    }

    items
}

fn cpp_keyword_items(prefix: &str) -> Vec<Value> {
    let prefix = prefix.trim();
    if prefix.len() < 2 || prefix.chars().any(|ch| ch.is_ascii_uppercase()) {
        return Vec::new();
    }

    let keywords = [
        "auto",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "constexpr",
        "continue",
        "do",
        "else",
        "enum",
        "explicit",
        "false",
        "final",
        "for",
        "if",
        "inline",
        "mutable",
        "namespace",
        "noexcept",
        "nullptr",
        "override",
        "private",
        "protected",
        "public",
        "return",
        "static",
        "struct",
        "switch",
        "template",
        "this",
        "true",
        "try",
        "typename",
        "using",
        "virtual",
        "void",
        "while",
    ];

    keywords
        .iter()
        .filter(|keyword| keyword.starts_with(prefix))
        .map(|keyword| keyword_item(keyword))
        .collect()
}

fn cpp_snippets(prefix: &str) -> Vec<Value> {
    let prefix = prefix.trim();
    if prefix.len() < 2 || prefix.chars().any(|ch| ch.is_ascii_uppercase()) {
        return Vec::new();
    }

    let snippets = [
        (
            "if",
            "if (${1:condition}) {\n\t$0\n}",
            "if block",
        ),
        (
            "ifelse",
            "if (${1:condition}) {\n\t$2\n} else {\n\t$0\n}",
            "if / else block",
        ),
        (
            "for",
            "for (int32 ${1:Index} = 0; ${1:Index} < ${2:Count}; ++${1:Index}) {\n\t$0\n}",
            "for loop",
        ),
        (
            "forr",
            "for (const auto& ${1:Item} : ${2:Items}) {\n\t$0\n}",
            "range-based for loop",
        ),
        (
            "while",
            "while (${1:condition}) {\n\t$0\n}",
            "while loop",
        ),
        (
            "switch",
            "switch (${1:value}) {\n\tcase ${2:Value}:\n\t\t$0\n\t\tbreak;\n\tdefault:\n\t\tbreak;\n}",
            "switch block",
        ),
        (
            "else",
            "else {\n\t$0\n}",
            "else block",
        ),
        (
            "elseif",
            "else if (${1:condition}) {\n\t$0\n}",
            "else if block",
        ),
        (
            "do",
            "do {\n\t$0\n} while (${1:condition});",
            "do / while loop",
        ),
        (
            "try",
            "try {\n\t$0\n} catch (const ${1:std::exception}& ${2:Exception}) {\n\t\n}",
            "try / catch block",
        ),
        (
            "return",
            "return ${1:value};",
            "return statement",
        ),
        (
            "namespace",
            "namespace ${1:Name}\n{\n\t$0\n}",
            "namespace block",
        ),
        (
            "class",
            "class ${1:Name}\n{\npublic:\n\t$0\n};",
            "class definition",
        ),
        (
            "struct",
            "struct ${1:Name}\n{\n\t$0\n};",
            "struct definition",
        ),
        (
            "lambda",
            "[${1:&}](${2}) {\n\t$0\n}",
            "lambda expression",
        ),
    ];

    snippets
        .iter()
        .filter(|(label, _, _)| label.starts_with(prefix))
        .map(|(label, insert_text, detail)| snippet(label, insert_text, detail))
        .collect()
}

fn function_completion_detail(
    return_type: Option<&str>,
    params: Option<&str>,
    owner_class: Option<&str>,
) -> String {
    let return_type = return_type
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .unwrap_or("function");
    let params = params
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .unwrap_or("()");

    let mut detail = format!("{} {}", return_type, params);
    if let Some(owner_class) = owner_class.map(|text| text.trim()).filter(|text| !text.is_empty()) {
        detail.push_str(" - ");
        detail.push_str(owner_class);
    }

    detail
}

fn function_snippet_text(name: &str, params: Option<&str>) -> String {
    let placeholders = function_param_placeholders(function_parameter_list(params.unwrap_or("()")));
    if placeholders.is_empty() {
        return format!("{}($1)", name);
    }

    format!("{}({})", name, placeholders.join(", "))
}

fn function_declaration_insert_text(
    return_type: Option<&str>,
    name: &str,
    detail: Option<&str>,
    add_override: bool,
) -> String {
    let return_type = return_type
        .map(|text| text.trim())
        .filter(|text| !text.is_empty());
    let params = function_parameter_list(detail.unwrap_or("()"));
    let suffix = normalize_override_suffix(function_signature_suffix(detail.unwrap_or("()")));

    let mut text = String::new();
    if add_override {
        text.push_str("virtual ");
    }
    if let Some(return_type) = return_type {
        text.push_str(return_type);
        text.push(' ');
    }
    text.push_str(name);
    text.push_str(params);

    if !suffix.is_empty() {
        text.push(' ');
        text.push_str(&suffix);
    }
    if add_override {
        text.push_str(" override");
    }
    text.push(';');
    text
}

fn function_parameter_list(detail: &str) -> &str {
    function_signature_span(detail)
        .map(|(start, end)| &detail[start..=end])
        .unwrap_or("()")
}

fn function_signature_suffix(detail: &str) -> &str {
    function_signature_span(detail)
        .map(|(_, end)| detail[end + 1..].trim())
        .unwrap_or("")
}

fn function_signature_span(text: &str) -> Option<(usize, usize)> {
    let start = text.find('(')?;
    let mut depth = 0usize;

    for (offset, ch) in text[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((start, start + offset));
                }
            }
            _ => {}
        }
    }

    None
}

fn normalize_override_suffix(suffix: &str) -> String {
    let suffix = suffix
        .split_once('=')
        .map(|(head, _)| head.trim())
        .unwrap_or_else(|| suffix.trim());

    suffix
        .split_whitespace()
        .filter(|token| *token != "override" && *token != "final")
        .collect::<Vec<_>>()
        .join(" ")
}

fn function_param_placeholders(params: &str) -> Vec<String> {
    let inner = params
        .trim()
        .strip_prefix('(')
        .and_then(|text| text.strip_suffix(')'))
        .unwrap_or(params)
        .trim();

    if inner.is_empty() || inner == "void" {
        return Vec::new();
    }

    split_top_level_params(inner)
        .into_iter()
        .filter(|param| !param.is_empty() && param != "void")
        .enumerate()
        .map(|(index, param)| format!("${{{}:{}}}", index + 1, snippet_escape(&param)))
        .collect()
}

fn split_top_level_params(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut angle = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut brace = 0usize;

    for (index, ch) in text.char_indices() {
        match ch {
            '<' => angle += 1,
            '>' => angle = angle.saturating_sub(1),
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            '{' => brace += 1,
            '}' => brace = brace.saturating_sub(1),
            ',' if angle == 0 && paren == 0 && bracket == 0 && brace == 0 => {
                let part = text[start..index].trim();
                if !part.is_empty() {
                    parts.push(part.to_string());
                }
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }

    let tail = text[start..].trim();
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }

    parts
}

fn snippet_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('$', "\\$").replace('}', "\\}")
}

/// Create one snippet item.
/// 创建一个 snippet 补全项。
fn snippet(label: &str, insert_text: &str, detail: &str) -> Value {
    json!({
        "label": label,
        "kind": 15,
        "detail": detail,
        "insertText": insert_text,
        "insertTextFormat": 2,
        "filterText": label,
        "sortText": completion_sort_text(900, 15, label),
        "score_offset": 18,
    })
}

fn keyword_item(label: &str) -> Value {
    json!({
        "label": label,
        "kind": 14,
        "detail": "keyword",
        "insertText": label,
        "filterText": label,
        "sortText": completion_sort_text(120, 14, label),
        "score_offset": 12,
    })
}

// -----------------------------------------------------------------------------
// Cache helpers
// -----------------------------------------------------------------------------

/// Read completion cache.
/// 读取补全缓存。
fn read_completion_cache(
    key: &str,
    memory_cache: &Option<Arc<Mutex<CompletionCache>>>,
    persistent_cache: &Option<Arc<Mutex<Connection>>>,
) -> Result<Option<Vec<Value>>> {
    if let Some(cache) = memory_cache {
        if let Some(value) = cache.lock().get(key, "") {
            if let Some(items) = value.as_array() {
                return Ok(Some(items.clone()));
            }
        }
    }

    if let Some(cache) = persistent_cache {
        let conn = cache.lock();

        let blob = conn
            .query_row(
                "SELECT value FROM persistent_cache WHERE key = ?",
                [key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;

        if let Some(blob) = blob {
            let value: Value = serde_json::from_slice(&blob)?;

            if let Some(items) = value.as_array() {
                return Ok(Some(items.clone()));
            }
        }
    }

    Ok(None)
}

/// Write completion cache.
/// 写入补全缓存。
fn write_completion_cache(
    key: &str,
    items: &[Value],
    memory_cache: &Option<Arc<Mutex<CompletionCache>>>,
    persistent_cache: &Option<Arc<Mutex<Connection>>>,
) {
    let value = json!(items);

    if let Some(cache) = memory_cache {
        cache.lock().put(key, "", value.clone());
    }

    if let Some(cache) = persistent_cache {
        if let Ok(blob) = serde_json::to_vec(&value) {
            let now = unix_timestamp();

            let _ = cache.lock().execute(
                "INSERT OR REPLACE INTO persistent_cache (key, value, last_used) VALUES (?, ?, ?)",
                params![key, blob, now],
            );
        }
    }
}

// -----------------------------------------------------------------------------
// Text helpers
// -----------------------------------------------------------------------------

/// Extract comment above a member declaration.
/// 提取成员声明上方的注释。
fn extract_comment_from_file(
    file_path: &str,
    line_number: usize,
    file_cache: &mut HashMap<String, Vec<String>>,
) -> String {
    if line_number == 0 {
        return String::new();
    }

    let Some(lines) = cached_file_lines(file_path, file_cache) else {
        return String::new();
    };

    let mut index = line_number.saturating_sub(1);
    let mut comments = Vec::new();
    let mut block_mode = false;

    while index > 0 {
        let text = lines[index - 1].trim();

        if text.is_empty()
            || text.starts_with("UPROPERTY")
            || text.starts_with("UFUNCTION")
            || text.starts_with("GENERATED_BODY")
        {
            index -= 1;
            continue;
        }

        if text.starts_with("//") {
            comments.push(text.trim_start_matches('/').trim().to_string());
            index -= 1;
            continue;
        }

        if text.ends_with("*/") {
            block_mode = true;
            comments.push(
                text.trim_end_matches("*/")
                    .trim_start_matches('*')
                    .trim()
                    .to_string(),
            );
            index -= 1;
            continue;
        }

        if block_mode {
            comments.push(
                text.trim_start_matches("/*")
                    .trim_start_matches('*')
                    .trim()
                    .to_string(),
            );

            if text.starts_with("/*") {
                break;
            }

            index -= 1;
            continue;
        }

        break;
    }

    comments.reverse();
    comments.into_iter().filter(|line| !line.is_empty()).collect::<Vec<_>>().join("\n")
}

#[derive(Clone, Debug)]
struct FunctionSignatureInfo {
    detail: String,
    is_static: bool,
}

fn resolve_function_signature_info(
    db_detail: Option<&str>,
    file_path: Option<&str>,
    line_number: Option<usize>,
    name: &str,
    file_cache: &mut HashMap<String, Vec<String>>,
) -> Option<FunctionSignatureInfo> {
    if let (Some(file_path), Some(line_number)) = (file_path, line_number) {
        if let Some(info) =
            extract_function_signature_from_file(file_path, line_number, name, file_cache)
        {
            return Some(info);
        }
    }

    db_detail
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|detail| FunctionSignatureInfo {
            detail: detail.to_string(),
            is_static: false,
        })
}

fn extract_function_signature_from_file(
    file_path: &str,
    line_number: usize,
    name: &str,
    file_cache: &mut HashMap<String, Vec<String>>,
) -> Option<FunctionSignatureInfo> {
    if line_number == 0 {
        return None;
    }

    let lines = cached_file_lines(file_path, file_cache)?;
    let start = line_number.saturating_sub(1).min(lines.len().saturating_sub(1));
    let mut text = String::new();
    let mut depth = 0usize;
    let mut saw_open = false;

    for line in lines.iter().skip(start).take(8) {
        let trimmed = line.trim();
        if trimmed.is_empty() && text.is_empty() {
            continue;
        }

        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(trimmed);

        for ch in trimmed.chars() {
            match ch {
                '(' => {
                    saw_open = true;
                    depth += 1;
                }
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }

        if saw_open && depth == 0 && (trimmed.contains(';') || trimmed.contains('{')) {
            break;
        }
    }

    let detail = function_detail_from_signature_text(&text, name)?;
    let is_static = function_prefix_from_signature_text(&text, name)
        .split_whitespace()
        .any(|token| token == "static");

    Some(FunctionSignatureInfo { detail, is_static })
}

fn function_detail_from_signature_text(signature: &str, name: &str) -> Option<String> {
    let prefix = function_prefix_from_signature_text(signature, name);
    let start = prefix.len() + name.len();
    if start > signature.len() {
        return None;
    }
    let tail = &signature[start..];
    let params = function_parameter_list(tail);
    let suffix = function_signature_suffix(tail)
        .split(['{', ';'])
        .next()
        .unwrap_or("")
        .trim();

    let mut detail = params.to_string();
    if !suffix.is_empty() {
        detail.push(' ');
        detail.push_str(suffix);
    }

    Some(detail)
}

fn function_prefix_from_signature_text<'a>(signature: &'a str, name: &str) -> &'a str {
    signature
        .find(name)
        .map(|index| &signature[..index])
        .unwrap_or("")
}

fn cached_file_lines<'a>(
    file_path: &str,
    file_cache: &'a mut HashMap<String, Vec<String>>,
) -> Option<&'a Vec<String>> {
    if !file_cache.contains_key(file_path) {
        let content = std::fs::read_to_string(file_path).ok()?;
        file_cache.insert(
            file_path.to_string(),
            content.lines().map(|line| line.to_string()).collect(),
        );
    }

    file_cache.get(file_path)
}

/// Merge comment documentation and DB detail.
/// 合并注释文档和 DB detail。
fn merge_docs(comment: String, detail: Option<String>) -> String {
    match (comment.is_empty(), detail) {
        (true, Some(detail)) => detail,
        (false, Some(detail)) if !detail.is_empty() => format!("{}\n\n{}", comment, detail),
        _ => comment,
    }
}

/// Clean C++/Unreal type text.
/// 清理 C++/Unreal 类型文本。
fn clean_type(raw: &str) -> String {
    let mut text = raw.trim().to_string();

    for keyword in [
        "const",
        "typename",
        "struct",
        "class",
        "enum",
        "virtual",
        "static",
        "inline",
        "FORCEINLINE",
        "volatile",
        "mutable",
    ] {
        text = text.replace(keyword, " ");
    }

    text = text.replace('*', " ").replace('&', " ");

    if let Some(inner) = extract_wrapped_type(&text) {
        return clean_type(&inner);
    }

    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract inner type from Unreal wrapper types.
/// 从 Unreal 包装类型中提取内部类型。
fn extract_wrapped_type(text: &str) -> Option<String> {
    let start = text.find('<')?;
    let end = text.rfind('>')?;
    let wrapper = text[..start].trim();

    if matches!(
        wrapper,
        "TObjectPtr"
            | "TWeakObjectPtr"
            | "TSoftObjectPtr"
            | "TSoftClassPtr"
            | "TSubclassOf"
            | "TSharedPtr"
            | "TSharedRef"
            | "TUniquePtr"
            | "TEnumAsByte"
    ) {
        return Some(text[start + 1..end].trim().to_string());
    }

    None
}

/// Extract type argument from template call text.
/// 从模板调用文本里提取类型参数。
fn extract_template_call_type(text: &str) -> Option<String> {
    let start = text.find('<')?;
    let end = text.rfind('>')?;

    Some(clean_type(&text[start + 1..end]))
}

/// Unwrap container element type.
/// 拆出容器元素类型。
fn unwrap_container_type(ty: &str) -> String {
    let Some(start) = ty.find('<') else {
        return clean_type(ty);
    };

    let Some(end) = ty.rfind('>') else {
        return clean_type(ty);
    };

    let wrapper = ty[..start].trim();
    let inner = &ty[start + 1..end];

    match wrapper {
        "TArray" | "TSet" => clean_type(inner),
        "TMap" => clean_type(template_argument(inner, 1)),
        _ => clean_type(ty),
    }
}

/// Get template argument by index.
/// 获取第 index 个模板参数。
fn template_argument(inner: &str, index: usize) -> &str {
    let mut depth = 0usize;
    let mut arg_index = 0usize;
    let mut start = 0usize;

    for (i, ch) in inner.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                if arg_index == index {
                    return inner[start..i].trim();
                }

                arg_index += 1;
                start = i + 1;
            }
            _ => {}
        }
    }

    if arg_index == index {
        inner[start..].trim()
    } else {
        ""
    }
}

/// Get enclosing class name.
/// 获取当前 node 所在 class 名。
fn enclosing_class(node: Node, content: &str) -> Option<String> {
    let fallback_offset = node.end_byte();
    let mut current = Some(node);

    while let Some(node) = current {
        match node.kind() {
            "class_specifier"
            | "struct_specifier"
            | "unreal_class_declaration"
            | "unreal_struct_declaration"
            | "unreal_reflected_class_declaration"
            | "unreal_reflected_struct_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    return Some(clean_type(node_text(name, content)));
                }
            }

            "function_definition" | "unreal_function_definition" => {
                if let Some(owner) = scope_name_from_declarator(node, content) {
                    return Some(owner);
                }
            }

            _ => {}
        }

        current = node.parent();
    }

    enclosing_class_from_text(content, fallback_offset)
}

fn scope_name_from_declarator(node: Node, content: &str) -> Option<String> {
    let decl = node.child_by_field_name("declarator")?;
    let owner = find_qualified_scope(decl)
        .map(|scope| clean_type(node_text(scope, content)))
        .or_else(|| scope_name_from_declarator_text(decl, content))?;

    if owner.is_empty() {
        None
    } else {
        Some(owner)
    }
}

fn scope_name_from_declarator_text(node: Node, content: &str) -> Option<String> {
    let text = node_text(node, content).trim();
    let head = text.split('(').next()?.trim();
    let scope = head.rsplit_once("::")?.0.trim();
    let owner = clean_type(scope);

    if owner.is_empty() {
        None
    } else {
        Some(owner)
    }
}

fn enclosing_class_from_text(content: &str, offset: usize) -> Option<String> {
    let before = &content[..offset.min(content.len())];
    let mut depth = 0usize;

    for (index, ch) in before.char_indices().rev() {
        match ch {
            '}' => depth += 1,
            '{' => {
                if depth == 0 {
                    if let Some(owner) = scope_name_before_block(&before[..index]) {
                        return Some(owner);
                    }
                } else {
                    depth -= 1;
                }
            }
            _ => {}
        }
    }

    None
}

fn scope_name_before_block(prefix: &str) -> Option<String> {
    let header_start = prefix
        .rfind(|ch| matches!(ch, ';' | '{' | '}'))
        .map(|index| index + 1)
        .unwrap_or(0);
    let signature = prefix[header_start..].trim();
    let head = signature.split('(').next()?.trim();
    let scope = head.rsplit_once("::")?.0.trim();
    let scope = scope
        .rsplit(|ch: char| ch.is_whitespace() || matches!(ch, '*' | '&'))
        .next()
        .unwrap_or(scope)
        .trim();
    let owner = clean_type(scope);

    if owner.is_empty() {
        None
    } else {
        Some(owner)
    }
}

/// Find scope inside qualified_identifier.
/// 查找 qualified_identifier 里的 scope。
fn find_qualified_scope(node: Node) -> Option<Node> {
    if node.kind() == "qualified_identifier" {
        return node.child_by_field_name("scope");
    }

    for child in node_children(node) {
        if let Some(found) = find_qualified_scope(child) {
            return Some(found);
        }
    }

    None
}

/// Get node text.
/// 获取 node 文本。
fn node_text<'a>(node: Node, content: &'a str) -> &'a str {
    node.utf8_text(content.as_bytes()).unwrap_or("")
}

/// Convert zero-based line and byte column to a byte offset.
/// 把 0-based 行号和字节列转换成全文 byte offset。
fn byte_offset_at(content: &str, line: usize, character: usize) -> Option<usize> {
    let mut offset = 0usize;

    for (row, text) in content.split_inclusive('\n').enumerate() {
        if row == line {
            return Some(offset + character.min(text.len()));
        }

        offset += text.len();
    }

    if line == content.lines().count() {
        Some(content.len())
    } else {
        None
    }
}

/// Collect node children.
/// 收集 node 子节点。
fn node_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

fn find_descendant_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }

    for child in node_children(node) {
        if let Some(found) = find_descendant_by_kind(child, kind) {
            return Some(found);
        }
    }

    None
}

/// Completion item kind mapping.
/// 补全 item kind 映射。
fn completion_kind(kind: &str) -> i64 {
    match kind {
        "function" => 2,
        "property" | "variable" | "field" => 5,
        "enum_item" => 20,
        _ => 1,
    }
}

/// Build a stable sort text for completion items.
/// 为补全项构造稳定的排序文本。
fn completion_sort_text(rank: usize, kind: i64, label: &str) -> String {
    format!(
        "{:04}_{:04}_{}",
        rank,
        kind,
        label.to_ascii_lowercase()
    )
}

/// Remove duplicate completion labels.
/// 去重补全项。
fn dedupe_completion_items(items: Vec<Value>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();

    for item in items {
        let Some(label) = item.get("label").and_then(|label| label.as_str()) else {
            continue;
        };

        if seen.insert(label.to_string()) {
            result.push(item);
        }
    }

    result
}

/// Get current Unix timestamp.
/// 获取当前 Unix 时间戳。
fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Resolve file_id by full path.
/// 根据完整路径解析 DB file_id。
fn get_file_id_by_full_path(conn: &Connection, file_path: &str) -> Option<i64> {
    let normalized = file_path.replace('\\', "/");

    let sql = r#"
        SELECT f.id
        FROM files f
        JOIN dir_paths dp ON f.directory_id = dp.id
        JOIN strings sf ON f.filename_id = sf.id
        WHERE
            CASE
                WHEN dp.full_path = '/' THEN '/' || sf.text
                WHEN substr(dp.full_path, -1) = '/' THEN dp.full_path || sf.text
                ELSE dp.full_path || '/' || sf.text
            END = ?
        LIMIT 1
    "#;

    conn.query_row(sql, [&normalized], |row| row.get::<_, i64>(0))
        .optional()
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    const TEST_FILE: &str = "C:/Project/Source/Game/MyActor.cpp";

    fn completion_at(conn: &Connection, source: &str) -> Vec<Value> {
        let (content, line, character) = source_with_cursor(source);
        process_completion(
            conn,
            &content,
            line,
            character,
            Some(TEST_FILE.to_string()),
            None,
            None,
        )
        .unwrap()
        .as_array()
        .cloned()
        .unwrap_or_default()
    }

    fn completion_at_with_engine(
        conn: &Connection,
        engine_conn: &Connection,
        source: &str,
    ) -> Vec<Value> {
        let (content, line, character) = source_with_cursor(source);
        process_completion_with_engine(
            conn,
            Some(engine_conn),
            &content,
            line,
            character,
            Some(TEST_FILE.to_string()),
            None,
            None,
        )
        .unwrap()
        .as_array()
        .cloned()
        .unwrap_or_default()
    }

    fn source_with_cursor(source: &str) -> (String, u32, u32) {
        let marker = "/*cursor*/";
        let offset = source.find(marker).expect("fixture must contain cursor marker");
        let content = source.replacen(marker, "", 1);
        let before = &source[..offset];
        let line = before.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let character = before
            .rsplit_once('\n')
            .map(|(_, tail)| tail.len())
            .unwrap_or(before.len()) as u32;

        (content, line, character)
    }

    fn labels(items: &[Value]) -> Vec<String> {
        items
            .iter()
            .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
            .map(|label| label.to_string())
            .collect()
    }

    fn has_label(items: &[Value], label: &str) -> bool {
        labels(items).iter().any(|item| item == label)
    }

    fn item_by_label<'a>(items: &'a [Value], label: &str) -> Option<&'a Value> {
        items.iter()
            .find(|item| item.get("label").and_then(|value| value.as_str()) == Some(label))
    }

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        let drive = insert_string(&conn, "C:");
        let project_name = insert_string(&conn, "Project");
        let source_name = insert_string(&conn, "Source");
        let game = insert_string(&conn, "Game");
        let public = insert_string(&conn, "Public");
        let file_name = insert_string(&conn, "MyActor.cpp");
        let header_name = insert_string(&conn, "MyActor.h");

        conn.execute(
            "INSERT INTO directories (parent_id, name_id) VALUES (NULL, ?)",
            [drive],
        )
        .unwrap();
        let c_dir = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO directories (parent_id, name_id) VALUES (?, ?)",
            [c_dir, project_name],
        )
        .unwrap();
        let project_dir = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO directories (parent_id, name_id) VALUES (?, ?)",
            [project_dir, source_name],
        )
        .unwrap();
        let source_dir = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO directories (parent_id, name_id) VALUES (?, ?)",
            [source_dir, game],
        )
        .unwrap();
        let game_dir = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO directories (parent_id, name_id) VALUES (?, ?)",
            [game_dir, public],
        )
        .unwrap();
        let public_dir = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO files (directory_id, filename_id, extension, is_header) VALUES (?, ?, 'cpp', 0)",
            [game_dir, file_name],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO files (directory_id, filename_id, extension, is_header) VALUES (?, ?, 'h', 1)",
            [public_dir, header_name],
        )
        .unwrap();

        let base_id = insert_class(&conn, "UBase", file_id);
        let actor_id = insert_class(&conn, "AMyActor", file_id);
        let widget_id = insert_class(&conn, "UMyWidget", file_id);

        insert_inheritance(&conn, actor_id, "UBase", base_id);
        insert_member(&conn, base_id, "ParentOnly", "function", Some("void"), "public", file_id);
        insert_member(&conn, actor_id, "LocalAction", "function", Some("void"), "public", file_id);
        insert_member(&conn, actor_id, "LocalValue", "property", Some("int"), "public", file_id);
        insert_member(&conn, widget_id, "WidgetAction", "function", Some("void"), "public", file_id);

        conn
    }

    fn insert_string(conn: &Connection, text: &str) -> i64 {
        conn.execute("INSERT OR IGNORE INTO strings (text) VALUES (?)", [text])
            .unwrap();
        conn.query_row("SELECT id FROM strings WHERE text = ?", [text], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn insert_class(conn: &Connection, name: &str, file_id: i64) -> i64 {
        let name_id = insert_string(conn, name);
        conn.execute(
            "INSERT INTO classes (name_id, file_id, line_number, symbol_type) VALUES (?, ?, 1, 'class')",
            [name_id, file_id],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_inheritance(conn: &Connection, child_id: i64, parent_name: &str, parent_id: i64) {
        let parent_name_id = insert_string(conn, parent_name);
        conn.execute(
            "INSERT INTO inheritance (child_id, parent_name_id, parent_class_id) VALUES (?, ?, ?)",
            [child_id, parent_name_id, parent_id],
        )
        .unwrap();
    }

    fn insert_external_inheritance(conn: &Connection, child_id: i64, parent_name: &str) {
        let parent_name_id = insert_string(conn, parent_name);
        conn.execute(
            "INSERT INTO inheritance (child_id, parent_name_id, parent_class_id) VALUES (?, ?, NULL)",
            [child_id, parent_name_id],
        )
        .unwrap();
    }

    fn insert_member(
        conn: &Connection,
        class_id: i64,
        name: &str,
        member_type: &str,
        return_type: Option<&str>,
        access: &str,
        file_id: i64,
    ) {
        let name_id = insert_string(conn, name);
        let type_id = insert_string(conn, member_type);
        let return_type_id = return_type.map(|text| insert_string(conn, text));

        conn.execute(
            "INSERT INTO members
             (class_id, name_id, type_id, access, return_type_id, line_number, file_id)
             VALUES (?, ?, ?, ?, ?, 1, ?)",
            rusqlite::params![class_id, name_id, type_id, access, return_type_id, file_id],
        )
        .unwrap();
    }

    fn insert_member_with_detail(
        conn: &Connection,
        class_id: i64,
        name: &str,
        member_type: &str,
        return_type: Option<&str>,
        detail: Option<&str>,
        access: &str,
        file_id: i64,
    ) {
        let name_id = insert_string(conn, name);
        let type_id = insert_string(conn, member_type);
        let return_type_id = return_type.map(|text| insert_string(conn, text));

        conn.execute(
            "INSERT INTO members
             (class_id, name_id, type_id, access, return_type_id, detail, line_number, file_id)
             VALUES (?, ?, ?, ?, ?, ?, 1, ?)",
            rusqlite::params![
                class_id,
                name_id,
                type_id,
                access,
                return_type_id,
                detail,
                file_id
            ],
        )
        .unwrap();
    }

    fn insert_member_with_access(
        conn: &Connection,
        class_id: i64,
        name: &str,
        member_type: &str,
        return_type: Option<&str>,
        access: Option<&str>,
        file_id: i64,
    ) {
        let name_id = insert_string(conn, name);
        let type_id = insert_string(conn, member_type);
        let return_type_id = return_type.map(|text| insert_string(conn, text));

        conn.execute(
            "INSERT INTO members
             (class_id, name_id, type_id, access, return_type_id, line_number, file_id)
             VALUES (?, ?, ?, ?, ?, 1, ?)",
            rusqlite::params![class_id, name_id, type_id, access, return_type_id, file_id],
        )
        .unwrap();
    }

    fn insert_header_under_header_dir(
        conn: &Connection,
        subdir: &str,
        filename: &str,
        extension: &str,
    ) -> i64 {
        let header_dir_id: i64 = conn
            .query_row(
                "SELECT directory_id FROM files WHERE is_header = 1 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let subdir_name_id = insert_string(conn, subdir);
        conn.execute(
            "INSERT INTO directories (parent_id, name_id) VALUES (?, ?)",
            [header_dir_id, subdir_name_id],
        )
        .unwrap();
        let subdir_id = conn.last_insert_rowid();

        let filename_id = insert_string(conn, filename);
        conn.execute(
            "INSERT INTO files (directory_id, filename_id, extension, is_header) VALUES (?, ?, ?, 1)",
            rusqlite::params![subdir_id, filename_id, extension],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn label_index(items: &[Value], label: &str) -> Option<usize> {
        items.iter().position(|item| item.get("label").and_then(|v| v.as_str()) == Some(label))
    }

    fn label_score_offset(items: &[Value], label: &str) -> Option<i64> {
        items.iter()
            .find(|item| item.get("label").and_then(|v| v.as_str()) == Some(label))
            .and_then(|item| item.get("score_offset"))
            .and_then(Value::as_i64)
    }

    #[test]
    fn member_completion_returns_members_without_snippets() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
class AMyActor {
public:
    void Test() {
        this->/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&items, "LocalAction"));
        assert!(has_label(&items, "LocalValue"));
        assert!(has_label(&items, "ParentOnly"));
        assert!(!has_label(&items, "UPROPERTY"));
        assert!(!has_label(&items, "Cast"));
    }

    #[test]
    fn macro_completion_filters_specifiers_by_prefix() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
UCLASS(Blue/*cursor*/)
class AMyActor {};
"#,
        );

        assert!(has_label(&items, "Blueprintable"));
        assert!(has_label(&items, "BlueprintType"));
        assert!(!has_label(&items, "Abstract"));
        assert!(!has_label(&items, "UCLASS"));
    }

    #[test]
    fn plain_lowercase_prefix_does_not_offer_ue_snippets() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
void Test() {
    in/*cursor*/
}
"#,
        );

        assert!(!has_label(&items, "INVTEXT"));
        assert!(!has_label(&items, "IMPLEMENT_MODULE"));
        assert!(!has_label(&items, "UPROPERTY"));
    }

    #[test]
    fn unreal_like_prefix_offers_snippets() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
void Test() {
    UPR/*cursor*/
}
"#,
        );

        assert!(has_label(&items, "UPROPERTY"));
        let property = items
            .iter()
            .find(|item| item.get("label").and_then(|label| label.as_str()) == Some("UPROPERTY"))
            .unwrap();
        assert_eq!(
            property
                .get("insertTextFormat")
                .and_then(|value| value.as_i64()),
            Some(2)
        );
    }

    #[test]
    fn include_context_returns_header_paths() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
#include "My/*cursor*/"
"#,
        );

        assert!(has_label(&items, "MyActor.h"));
        assert!(!has_label(&items, "UPROPERTY"));
    }

    #[test]
    fn include_context_merges_engine_header_paths() {
        let project_conn = test_db();
        let engine_conn = test_db();
        insert_header_under_header_dir(&engine_conn, "GameFramework", "Actor.h", "h");

        let items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
#include "GameFramework/*cursor*/"
"#,
        );

        assert!(has_label(&items, "GameFramework/Actor.h"));
    }

    #[test]
    fn include_context_suggests_engine_directories() {
        let project_conn = test_db();
        let engine_conn = test_db();
        insert_header_under_header_dir(&engine_conn, "GameFramework", "Actor.h", "h");

        let items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
#include "GameFra/*cursor*/"
"#,
        );

        assert!(has_label(&items, "GameFramework/"));
    }

    #[test]
    fn function_completion_inserts_signature_snippet() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
void HelperAbility(int32 Count, const FString& Name);

void Test()
{
    Help/*cursor*/
}
"#,
        );

        let helper = item_by_label(&items, "HelperAbility").unwrap();
        assert_eq!(helper.get("insertTextFormat").and_then(Value::as_i64), Some(2));
        let insert_text = helper.get("insertText").and_then(Value::as_str).unwrap();
        assert!(insert_text.contains("HelperAbility("));
        assert!(insert_text.contains("Count"));
        assert!(insert_text.contains("Name"));
        assert!(insert_text.contains("${1:"));
    }

    #[test]
    fn function_member_completion_inserts_signature_snippet() {
        let conn = test_db();
        let actor_id: i64 = conn
            .query_row(
                "SELECT c.id FROM classes c JOIN strings s ON c.name_id = s.id WHERE s.text = 'AMyActor' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        insert_member(
            &conn,
            actor_id,
            "DoAction",
            "function",
            Some("void"),
            "public",
            file_id,
        );

        let items = completion_at(
            &conn,
            r#"
class AMyActor {
public:
    void Test() {
        DoAct/*cursor*/
    }
};
"#,
        );

        let action = item_by_label(&items, "DoAction").unwrap();
        assert_eq!(action.get("insertTextFormat").and_then(Value::as_i64), Some(2));
        assert!(action.get("detail").and_then(Value::as_str).unwrap_or("").contains("void"));
        assert!(action
            .get("insertText")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("DoAction("));
    }

    #[test]
    fn preprocessor_directive_completion_offers_include() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
#inc/*cursor*/
"#,
        );

        assert!(has_label(&items, "include"));
        let include = items
            .iter()
            .find(|item| item.get("label").and_then(|label| label.as_str()) == Some("include"))
            .unwrap();
        assert_eq!(
            include
                .get("insertTextFormat")
                .and_then(|value| value.as_i64()),
            Some(2)
        );
    }

    #[test]
    fn keyword_completion_offers_common_cpp_keywords() {
        let conn = test_db();

        let return_items = completion_at(
            &conn,
            r#"
void Test() {
    ret/*cursor*/
}
"#,
        );
        assert!(has_label(&return_items, "return"));

        let virtual_items = completion_at(
            &conn,
            r#"
vi/*cursor*/
"#,
        );
        assert!(has_label(&virtual_items, "virtual"));

        let void_items = completion_at(
            &conn,
            r#"
vo/*cursor*/
"#,
        );
        assert!(has_label(&void_items, "void"));
    }

    #[test]
    fn control_flow_completion_prefers_block_snippets() {
        let conn = test_db();

        let if_items = completion_at(
            &conn,
            r#"
void Test() {
    if/*cursor*/
}
"#,
        );

        let if_item = item_by_label(&if_items, "if").unwrap();
        assert_eq!(if_item.get("insertTextFormat").and_then(Value::as_i64), Some(2));
        assert_eq!(
            if_item.get("insertText").and_then(Value::as_str),
            Some("if (${1:condition}) {\n\t$0\n}")
        );

        let for_items = completion_at(
            &conn,
            r#"
void Test() {
    fo/*cursor*/
}
"#,
        );

        let for_item = item_by_label(&for_items, "for").unwrap();
        assert_eq!(for_item.get("insertTextFormat").and_then(Value::as_i64), Some(2));
        assert!(
            for_item
                .get("insertText")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("for (int32 ${1:Index} = 0;")
        );

        let range_items = completion_at(
            &conn,
            r#"
void Test() {
    forr/*cursor*/
}
"#,
        );

        let range_item = item_by_label(&range_items, "forr").unwrap();
        assert_eq!(range_item.get("insertTextFormat").and_then(Value::as_i64), Some(2));
        assert_eq!(
            range_item.get("insertText").and_then(Value::as_str),
            Some("for (const auto& ${1:Item} : ${2:Items}) {\n\t$0\n}")
        );

        let namespace_items = completion_at(
            &conn,
            r#"
name/*cursor*/
"#,
        );

        let namespace_item = item_by_label(&namespace_items, "namespace").unwrap();
        assert_eq!(namespace_item.get("insertTextFormat").and_then(Value::as_i64), Some(2));
        assert!(
            namespace_item
                .get("insertText")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("namespace ${1:Name}")
        );
    }

    #[test]
    fn meta_context_returns_metadata_keys() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
UPROPERTY(meta=(Allow/*cursor*/))
int32 Value;
"#,
        );

        assert!(has_label(&items, "AllowPrivateAccess"));
        assert!(!has_label(&items, "EditAnywhere"));
    }

    #[test]
    fn auto_cast_initializer_drives_member_completion() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
void Test(UObject* Object) {
    auto Widget = Cast<UMyWidget>(Object);
    Widget->/*cursor*/
}
"#,
        );

        assert!(has_label(&items, "WidgetAction"));
        assert!(!has_label(&items, "LocalAction"));
    }

    #[test]
    fn member_completion_keeps_prefix_matches_before_middle_matches() {
        let conn = test_db();
        let actor_id: i64 = conn
            .query_row(
                "SELECT c.id FROM classes c JOIN strings s ON c.name_id = s.id WHERE s.text = 'AMyActor' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        insert_member(&conn, actor_id, "GetActorLocation", "function", Some("FVector"), "public", file_id);
        insert_member(&conn, actor_id, "GetSocketActor", "function", Some("AActor"), "public", file_id);

        let items = completion_at(
            &conn,
            r#"
class AMyActor {
public:
    void Test() {
        this->GetActor/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&items, "GetActorLocation"));
        assert!(has_label(&items, "GetSocketActor"));

        let prefix_index = label_index(&items, "GetActorLocation").unwrap();
        let middle_index = label_index(&items, "GetSocketActor").unwrap();
        assert!(prefix_index < middle_index);
    }

    #[test]
    fn global_completion_matches_middle_words() {
        let conn = test_db();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        insert_class(&conn, "GetActorComponent", file_id);
        insert_class(&conn, "GetMeshActor", file_id);

        let items = completion_at(
            &conn,
            r#"
void Test() {
    GetActor/*cursor*/
}
"#,
        );

        assert!(has_label(&items, "GetActorComponent"));
        assert!(has_label(&items, "GetMeshActor"));

        let prefix_index = label_index(&items, "GetActorComponent").unwrap();
        let middle_index = label_index(&items, "GetMeshActor").unwrap();
        assert!(prefix_index < middle_index);
        assert!(
            label_score_offset(&items, "GetActorComponent").unwrap()
                > label_score_offset(&items, "GetMeshActor").unwrap()
        );
    }

    #[test]
    fn global_completion_does_not_surface_missing_get_prefix_candidates() {
        let conn = test_db();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        insert_class(&conn, "GetAbilitySystemComponent", file_id);
        insert_class(&conn, "AbilitySystemComponent", file_id);

        let items = completion_at(
            &conn,
            r#"
void Test() {
    GetAbilitySystem/*cursor*/
}
"#,
        );

        assert!(has_label(&items, "GetAbilitySystemComponent"));
        assert!(!has_label(&items, "AbilitySystemComponent"));
    }

    #[test]
    fn class_scope_completion_prefers_inherited_members_without_global_noise() {
        let conn = test_db();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();
        let base_id: i64 = conn
            .query_row(
                "SELECT c.id FROM classes c JOIN strings s ON c.name_id = s.id WHERE s.text = 'UBase' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();

        insert_member(&conn, base_id, "GetAbilitySource", "function", Some("UObject"), "public", file_id);
        insert_class(&conn, "GetActorLocation", file_id);
        insert_class(&conn, "GetAbilitySystemComponent", file_id);

        let items = completion_at(
            &conn,
            r#"
class AMyActor : public UBase {
public:
    void Test() {
        GetA/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&items, "GetAbilitySource"));
        assert!(!has_label(&items, "GetActorLocation"));
        assert!(!has_label(&items, "GetAbilitySystemComponent"));
    }

    #[test]
    fn single_word_completion_can_still_match_middle_word_candidates() {
        let conn = test_db();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        insert_class(&conn, "GetMeshActor", file_id);

        let items = completion_at(
            &conn,
            r#"
void Test() {
    Actor/*cursor*/
}
"#,
        );

        assert!(has_label(&items, "GetMeshActor"));
    }

    #[test]
    fn local_completion_returns_parameters_and_locals() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
void Test(UAbilitySystemComponent* AbilitySystem)
{
    auto Widget = CreateWidget<UMyWidget>(Object);
    Abili/*cursor*/
}
"#,
        );

        assert!(has_label(&items, "AbilitySystem"));

        let widget_items = completion_at(
            &conn,
            r#"
void Test(UAbilitySystemComponent* AbilitySystem)
{
    auto Widget = CreateWidget<UMyWidget>(Object);
    Widg/*cursor*/
}
"#,
        );

        assert!(has_label(&widget_items, "Widget"));
    }

    #[test]
    fn buffer_completion_returns_free_functions() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
void HelperAbility();

void Test()
{
    Help/*cursor*/
}
"#,
        );

        assert!(has_label(&items, "HelperAbility"));
    }

    #[test]
    fn engine_parent_members_extend_project_completion() {
        let project_conn = test_db();
        let engine_conn = test_db();

        let file_id: i64 = project_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();
        let engine_file_id: i64 = engine_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        let ability_child_id = insert_class(&project_conn, "UMyAbility", file_id);
        insert_external_inheritance(&project_conn, ability_child_id, "UGameplayAbility");

        let gameplay_ability_id = insert_class(&engine_conn, "UGameplayAbility", engine_file_id);
        insert_member(
            &engine_conn,
            gameplay_ability_id,
            "EndAbility",
            "function",
            Some("void"),
            "protected",
            engine_file_id,
        );
        insert_member(
            &engine_conn,
            gameplay_ability_id,
            "AbilitySystem",
            "property",
            Some("UAbilitySystemComponent"),
            "protected",
            engine_file_id,
        );

        let asc_id = insert_class(&engine_conn, "UAbilitySystemComponent", engine_file_id);
        insert_member(
            &engine_conn,
            asc_id,
            "CancelAllAbilities",
            "function",
            Some("void"),
            "public",
            engine_file_id,
        );

        let items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
class UMyAbility : public UGameplayAbility
{
public:
    void Test()
    {
        EndAb/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&items, "EndAbility"));

        let member_items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
class UMyAbility : public UGameplayAbility
{
public:
    void Test()
    {
        AbilitySystem->Cancel/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&member_items, "CancelAllAbilities"));
    }

    #[test]
    fn current_buffer_inheritance_surfaces_engine_parent_members() {
        let project_conn = test_db();
        let engine_conn = test_db();

        let engine_file_id: i64 = engine_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        let gameplay_ability_id = insert_class(&engine_conn, "UGameplayAbility", engine_file_id);
        insert_member(
            &engine_conn,
            gameplay_ability_id,
            "EndAbility",
            "function",
            Some("void"),
            "protected",
            engine_file_id,
        );
        insert_member(
            &engine_conn,
            gameplay_ability_id,
            "AbilitySystemComponent",
            "function",
            Some("UAbilitySystemComponent"),
            "protected",
            engine_file_id,
        );

        let asc_id = insert_class(&engine_conn, "UAbilitySystemComponent", engine_file_id);
        insert_member(
            &engine_conn,
            asc_id,
            "CancelAllAbilities",
            "function",
            Some("void"),
            "public",
            engine_file_id,
        );

        let items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
class UAbility_Death : public UGameplayAbility
{
public:
    void Test()
    {
        EndAb/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&items, "EndAbility"));

        let member_items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
class UAbility_Death : public UGameplayAbility
{
public:
    void Test()
    {
        AbilitySystemComponent()->Cancel/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&member_items, "CancelAllAbilities"));
    }

    #[test]
    fn out_of_class_definition_surfaces_engine_parent_members() {
        let project_conn = test_db();
        let engine_conn = test_db();

        let file_id: i64 = project_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();
        let engine_file_id: i64 = engine_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        let ability_child_id = insert_class(&project_conn, "UMyAbility", file_id);
        insert_external_inheritance(&project_conn, ability_child_id, "UGameplayAbility");

        let gameplay_ability_id = insert_class(&engine_conn, "UGameplayAbility", engine_file_id);
        insert_member(
            &engine_conn,
            gameplay_ability_id,
            "EndAbility",
            "function",
            Some("void"),
            "protected",
            engine_file_id,
        );

        let items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
void UMyAbility::ActivateAbility()
{
    EndAb/*cursor*/
}
"#,
        );

        assert!(has_label(&items, "EndAbility"));
    }

    #[test]
    fn out_of_class_definition_infers_enclosing_class() {
        let source = r#"
void UMyAbility::ActivateAbility()
{
    EndAb/*cursor*/
}
"#;
        let (content, line, character) = source_with_cursor(source);

        let mut parser = Parser::new();
        let language: tree_sitter::Language = tree_sitter_unreal_cpp::LANGUAGE.into();
        parser.set_language(&language).unwrap();

        let tree = parser.parse(&content, None).unwrap();
        let node = cursor_node(tree.root_node(), line, character).unwrap();
        assert_eq!(enclosing_class(node, &content).as_deref(), Some("UMyAbility"));
    }

    #[test]
    fn engine_impl_members_are_available_in_completion() {
        let project_conn = test_db();
        let engine_conn = test_db();

        let file_id: i64 = project_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();
        let engine_file_id: i64 = engine_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        let ability_child_id = insert_class(&project_conn, "UMyAbility", file_id);
        insert_external_inheritance(&project_conn, ability_child_id, "UGameplayAbility");

        let gameplay_ability_id = insert_class(&engine_conn, "UGameplayAbility", engine_file_id);
        insert_member_with_access(
            &engine_conn,
            gameplay_ability_id,
            "GetCurrentAbilitySpec",
            "function",
            Some("FGameplayAbilitySpec"),
            Some("impl"),
            engine_file_id,
        );

        let items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
class UMyAbility : public UGameplayAbility
{
public:
    void Test()
    {
        GetCur/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&items, "GetCurrentAbilitySpec"));
    }

    #[test]
    fn current_buffer_inheritance_surfaces_custom_parent_members() {
        let conn = test_db();
        let file_id: i64 = conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        let base_id = insert_class(&conn, "UMyGameplayAbilityBase", file_id);
        insert_member(
            &conn,
            base_id,
            "ActivateCombo",
            "function",
            Some("void"),
            "protected",
            file_id,
        );

        let items = completion_at(
            &conn,
            r#"
class UAbility_Death : public UMyGameplayAbilityBase
{
public:
    void Test()
    {
        ActivateC/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&items, "ActivateCombo"));

        let member_items = completion_at(
            &conn,
            r#"
class UAbility_Death : public UMyGameplayAbilityBase
{
public:
    void Test()
    {
        UAbility_Death* Ability = this;
        Ability->ActivateC/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&member_items, "ActivateCombo"));
    }

    #[test]
    fn class_scope_completion_surfaces_engine_parent_members() {
        let project_conn = test_db();
        let engine_conn = test_db();

        let engine_file_id: i64 = engine_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        let actor_id = insert_class(&engine_conn, "AActor", engine_file_id);
        insert_member(
            &engine_conn,
            actor_id,
            "PostInitializeComponents",
            "function",
            Some("void"),
            "protected",
            engine_file_id,
        );

        let items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
class AMyActor : public AActor
{
public:
    PostI/*cursor*/
};
"#,
        );

        assert!(has_label(&items, "PostInitializeComponents"));
    }

    #[test]
    fn class_scope_completion_surfaces_header_function_declarations() {
        let conn = test_db();
        let items = completion_at(
            &conn,
            r#"
class AMyActor
{
public:
    void HeaderOnlyAction();

    void Test()
    {
        HeaderO/*cursor*/
    }
};
"#,
        );

        assert!(has_label(&items, "HeaderOnlyAction"));
    }

    #[test]
    fn reflected_class_scope_completion_surfaces_engine_parent_members() {
        let project_conn = test_db();
        let engine_conn = test_db();

        let file_id: i64 = project_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();
        let engine_file_id: i64 = engine_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        let child_id = insert_class(&project_conn, "UMyAbility", file_id);
        insert_external_inheritance(&project_conn, child_id, "UGameplayAbility");

        let gameplay_ability_id = insert_class(&engine_conn, "UGameplayAbility", engine_file_id);
        insert_member(
            &engine_conn,
            gameplay_ability_id,
            "CancelAllAbilities",
            "function",
            Some("void"),
            "protected",
            engine_file_id,
        );

        let items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
UCLASS()
class UMyAbility : public UGameplayAbility
{
    GENERATED_BODY()

public:
    Canc/*cursor*/
};
"#,
        );

        assert!(has_label(&items, "CancelAllAbilities"));
    }

    #[test]
    fn class_scope_function_completion_inserts_override_declaration() {
        let project_conn = test_db();
        let engine_conn = test_db();

        let file_id: i64 = project_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();
        let engine_file_id: i64 = engine_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        let child_id = insert_class(&project_conn, "UMyHero", file_id);
        insert_external_inheritance(&project_conn, child_id, "UGameplayAbility");

        let parent_id = insert_class(&engine_conn, "UGameplayAbility", engine_file_id);
        insert_member_with_detail(
            &engine_conn,
            parent_id,
            "GetAbilitySystemComponent",
            "function",
            Some("UAbilitySystemComponent*"),
            Some("() const"),
            "protected",
            engine_file_id,
        );

        let items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
class UMyHero : public UGameplayAbility
{
public:
    GetAbility/*cursor*/
};
"#,
        );

        let item = item_by_label(&items, "GetAbilitySystemComponent").unwrap();
        assert_eq!(item.get("insertTextFormat").and_then(Value::as_i64), Some(1));
        assert_eq!(
            item.get("insertText").and_then(Value::as_str),
            Some("virtual UAbilitySystemComponent* GetAbilitySystemComponent() const override;"),
        );
    }

    #[test]
    fn class_scope_function_completion_replaces_existing_virtual_return_prefix() {
        let project_conn = test_db();
        let engine_conn = test_db();

        let file_id: i64 = project_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();
        let engine_file_id: i64 = engine_conn
            .query_row("SELECT id FROM files WHERE extension = 'cpp' LIMIT 1", [], |row| row.get(0))
            .unwrap();

        let child_id = insert_class(&project_conn, "UMyHero", file_id);
        insert_external_inheritance(&project_conn, child_id, "UGameplayAbility");

        let parent_id = insert_class(&engine_conn, "UGameplayAbility", engine_file_id);
        insert_member_with_detail(
            &engine_conn,
            parent_id,
            "GetAbilitySystemComponent",
            "function",
            Some("UAbilitySystemComponent*"),
            Some("() const"),
            "protected",
            engine_file_id,
        );

        let items = completion_at_with_engine(
            &project_conn,
            &engine_conn,
            r#"
class UMyHero : public UGameplayAbility
{
public:
    virtual UAbilitySystemComponent* GetAbility/*cursor*/
};
"#,
        );

        let item = item_by_label(&items, "GetAbilitySystemComponent").unwrap();
        let text_edit = item.get("textEdit").unwrap();
        assert_eq!(
            text_edit.get("newText").and_then(Value::as_str),
            Some("virtual UAbilitySystemComponent* GetAbilitySystemComponent() const override;"),
        );
        assert_eq!(
            text_edit
                .get("range")
                .and_then(|range| range.get("start"))
                .and_then(|start| start.get("character"))
                .and_then(Value::as_u64),
            Some(4),
        );
    }

    #[test]
    fn declaration_completion_start_prefers_return_type_prefix() {
        let source = r#"    UAbilitySystemComponent* GetAbility"#;
        let start = declaration_completion_start(
            source,
            0,
            source.len(),
            "GetAbility",
            "GetAbilitySystemComponent",
            "virtual UAbilitySystemComponent* GetAbilitySystemComponent() const override;",
        );

        assert_eq!(start, 4);
    }

    #[test]
    fn function_declaration_insert_text_preserves_const_suffix() {
        assert_eq!(
            function_declaration_insert_text(
                Some("UAbilitySystemComponent*"),
                "GetAbilitySystemComponent",
                Some("() const"),
                true,
            ),
            "virtual UAbilitySystemComponent* GetAbilitySystemComponent() const override;",
        );
    }
}
