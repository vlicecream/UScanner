use std::cell::RefCell;
use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Context;
use memmap2::Mmap;
use regex::Regex;
use sha2::{Digest, Sha256};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::types::{ClassInfo, InputFile, MemberInfo, ParseData, ParseResult};

/// Regexes reused while cleaning C++ / Unreal type prefixes.
/// 清理 C++ / Unreal 类型前缀时复用的正则，避免每次解析都重新编译。
struct CleanRegexes {
    keywords: Vec<Regex>,
    api: Regex,
    unreal_macros: Regex,
    whitespace: Regex,
}

/// Lazily initialized regex cache.
/// 懒加载的正则缓存。
static CLEAN_REGEXES: OnceLock<CleanRegexes> = OnceLock::new();

fn get_clean_regexes() -> &'static CleanRegexes {
    CLEAN_REGEXES.get_or_init(|| {
        let keywords = [
            "virtual",
            "static",
            "inline",
            "FORCEINLINE",
            "FORCEINLINE_DEBUGGABLE",
            "constexpr",
            "const",
            "friend",
            "class",
            "struct",
            "enum",
            "typename",
        ];

        CleanRegexes {
            keywords: keywords
                .iter()
                .map(|kw| Regex::new(&format!(r"\b{}\b", regex::escape(kw))).unwrap())
                .collect(),

            // Matches module export macros such as MYGAME_API.
            // 匹配 Unreal 模块导出宏，例如 MYGAME_API。
            api: Regex::new(r"\b[A-Z0-9_]+_API\b").unwrap(),

            // Matches common Unreal reflection macros before return/property types.
            // 匹配返回类型或属性类型前面的 Unreal 反射宏。
            unreal_macros: Regex::new(
                r"\bU(?:CLASS|STRUCT|ENUM|FUNCTION|PROPERTY|INTERFACE|DELEGATE|META)\s*\([^)]*\)",
            )
            .unwrap(),

            whitespace: Regex::new(r"\s+").unwrap(),
        }
    })
}

thread_local! {
    /// Per-thread parser reused across files.
    /// 每个线程复用一个 parser，减少重复分配。
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());

    /// Per-thread query cursor reused across files.
    /// 每个线程复用一个 query cursor。
    static CURSOR: RefCell<QueryCursor> = RefCell::new(QueryCursor::new());
}

/// Main symbol query for Unreal C++.
/// Unreal C++ 主符号查询。
///
/// This query extracts:
/// - class / struct / enum definitions
/// - Unreal reflected declarations
/// - base classes
/// - functions and fields
/// - enum items
/// - function calls and member calls
///
/// 这个 query 用来提取：
/// - class / struct / enum 定义
/// - Unreal 反射声明
/// - 父类
/// - 函数和字段
/// - 枚举项
/// - 函数调用和成员调用
pub const QUERY_STR: &str = r#"
  ; ========================
  ; Type definitions
  ; 类型定义
  ; ========================

  (class_specifier
    name: (type_identifier) @class_name) @class_def

  (struct_specifier
    name: (type_identifier) @struct_name) @struct_def

  (enum_specifier
    name: (type_identifier) @enum_name) @enum_def

  ; UTreeSitter reflected declarations.
  ; UTreeSitter 的 Unreal 反射声明节点。

  (unreal_reflected_class_declaration
    name: [
      (type_identifier) @class_name
      (qualified_identifier) @class_name
    ]) @uclass_def

  (unreal_reflected_struct_declaration
    name: [
      (type_identifier) @struct_name
      (qualified_identifier) @struct_name
    ]) @ustruct_def

  (unreal_reflected_enum_declaration
    name: [
      (type_identifier) @enum_name
      (qualified_identifier) @enum_name
    ]) @uenum_def

  ; Base classes.
  ; 父类列表。
  (base_class_clause
    (access_specifier)?
    [
      (type_identifier) @base_class_name
      (qualified_identifier) @base_class_name
    ])

  ; ========================
  ; Members and functions
  ; 成员和函数
  ; ========================

  (function_definition) @func_node
  (declaration) @decl_node
  (field_declaration) @field_node
  (unreal_function_declaration) @ufunc_node

  (enumerator
    name: (identifier) @enum_val_name) @enum_item

  ; ========================
  ; Calls
  ; 调用
  ; ========================

  (call_expression
    function: [
      (identifier) @call_name
      (qualified_identifier
        name: (identifier) @call_name)
      (field_expression
        field: (field_identifier) @call_name)
      (field_expression
        field: (template_method
          name: (field_identifier) @call_name))
      (template_function
        name: (identifier) @call_name)
    ]) @call_expr

  (field_expression
    field: (field_identifier) @field_name) @field_expr
"#;

/// Include query kept separate because includes are used for dependency graphing.
/// include query 单独保留，因为 include 通常用于依赖图分析。
pub const INCLUDE_QUERY_STR: &str = r#"
  (preproc_include
    path: [
      (string_literal) @path
      (system_lib_string) @path
    ]) @include
"#;

/// Parse one file and return structured symbol data.
/// 解析单个文件并返回结构化符号数据。
pub fn process_file(
    input: &InputFile,
    language: &tree_sitter::Language,
    query: &Query,
    include_query: &Query,
) -> anyhow::Result<ParseResult> {
    let file = File::open(&input.path)
        .with_context(|| format!("failed to open {}", input.path))?;

    // Memory-map the file to avoid copying large source files into memory.
    // 使用 mmap 读取文件，避免把大文件重复复制到内存。
    let mmap = unsafe { Mmap::map(&file) }
        .with_context(|| format!("failed to mmap {}", input.path))?;

    let content_bytes = &mmap[..];

    // Content hash is used to skip unchanged files.
    // 通过内容 hash 跳过未变化的文件。
    let mut hasher = Sha256::new();
    hasher.update(content_bytes);
    let new_hash = hex::encode(hasher.finalize());

    if input.old_hash.as_ref() == Some(&new_hash) {
        return Ok(ParseResult {
            path: input.path.clone(),
            status: "cache_hit".to_string(),
            mtime: input.mtime,
            data: None,
            module_id: input.module_id,
        });
    }

    let ext = Path::new(&input.path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let is_header = matches!(ext.as_str(), "h" | "hpp" | "hh" | "inl");

    // Header files are numerous in Unreal projects, so quickly skip headers
    // that do not contain useful Unreal/C++ indexing markers.
    // Unreal 项目头文件很多，所以先快速跳过没有关键标记的头文件。
    if is_header && !looks_like_interesting_unreal_header(content_bytes) {
        return Ok(ParseResult {
            path: input.path.clone(),
            status: "parsed".to_string(),
            mtime: input.mtime,
            data: Some(ParseData {
                classes: Vec::new(),
                calls: Vec::new(),
                includes: Vec::new(),
                parser: "fast-skip".to_string(),
                new_hash,
            }),
            module_id: input.module_id,
        });
    }

    let (classes, calls, includes) =
        parse_content_mmap(content_bytes, &input.path, language, query, include_query)?;

    Ok(ParseResult {
        path: input.path.clone(),
        status: "parsed".to_string(),
        mtime: input.mtime,
        data: Some(ParseData {
            classes,
            calls,
            includes,
            parser: "treesitter".to_string(),
            new_hash,
        }),
        module_id: input.module_id,
    })
}

/// Parse already-loaded bytes.
/// 解析已经加载到内存中的源码字节。
pub fn parse_content_mmap(
    content_bytes: &[u8],
    _path: &str,
    language: &tree_sitter::Language,
    query: &Query,
    include_query: &Query,
) -> anyhow::Result<(
    Vec<ClassInfo>,
    Vec<crate::types::CallInfo>,
    Vec<String>,
)> {
    PARSER.with(|p_cell| {
        let mut parser = p_cell.borrow_mut();
        parser
            .set_language(language)
            .context("failed to set tree-sitter language")?;

        let tree = parser
            .parse(content_bytes, None)
            .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        let root = tree.root_node();

        CURSOR.with(|c_cell| {
            let mut cursor = c_cell.borrow_mut();

            let mut classes: Vec<ClassInfo> = Vec::new();
            let mut calls: Vec<crate::types::CallInfo> = Vec::new();
            let mut includes: Vec<String> = Vec::new();
            let mut pending_members: Vec<(MemberInfo, usize, usize)> = Vec::new();

            collect_includes(&mut cursor, include_query, root, content_bytes, &mut includes);
            collect_symbols(
                &mut cursor,
                query,
                root,
                content_bytes,
                &mut classes,
                &mut calls,
                &mut pending_members,
            );

            attach_pending_members(&mut classes, pending_members);

            Ok((classes, calls, includes))
        })
    })
}

/// Convenience parser for string content.
/// 用于测试或内存字符串的便捷解析入口。
pub fn parse_content(
    content: &str,
    path: &str,
    language: &tree_sitter::Language,
    query: &Query,
) -> anyhow::Result<(
    Vec<ClassInfo>,
    Vec<crate::types::CallInfo>,
    Vec<String>,
)> {
    let include_query = Query::new(language, INCLUDE_QUERY_STR)
        .context("failed to compile include query")?;

    parse_content_mmap(content.as_bytes(), path, language, query, &include_query)
}

/// Cheap header pre-filter.
/// 低成本头文件预过滤。
fn looks_like_interesting_unreal_header(content: &[u8]) -> bool {
    contains_bytes(content, b"#include")
        || contains_bytes(content, b"UCLASS")
        || contains_bytes(content, b"USTRUCT")
        || contains_bytes(content, b"UENUM")
        || contains_bytes(content, b"UINTERFACE")
        || contains_bytes(content, b"UDELEGATE")
        || contains_bytes(content, b"UFUNCTION")
        || contains_bytes(content, b"UPROPERTY")
        || contains_bytes(content, b"GENERATED_BODY")
        || contains_bytes(content, b"DECLARE_")
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Collect include paths.
/// 收集 include 路径。
fn collect_includes(
    cursor: &mut QueryCursor,
    include_query: &Query,
    root: Node,
    content_bytes: &[u8],
    includes: &mut Vec<String>,
) {
    let mut include_matches = cursor.matches(include_query, root, content_bytes);

    while let Some(m) = include_matches.next() {
        for cap in m.captures {
            if include_query.capture_names()[cap.index as usize] == "path" {
                let path = get_node_text(&cap.node, content_bytes)
                    .trim_matches('"')
                    .trim_matches('<')
                    .trim_matches('>')
                    .to_string();

                if !path.is_empty() {
                    includes.push(path);
                }
            }
        }
    }
}

/// Collect classes, members, calls, and enum items.
/// 收集类、成员、调用和枚举项。
fn collect_symbols(
    cursor: &mut QueryCursor,
    query: &Query,
    root: Node,
    content_bytes: &[u8],
    classes: &mut Vec<ClassInfo>,
    calls: &mut Vec<crate::types::CallInfo>,
    pending_members: &mut Vec<(MemberInfo, usize, usize)>,
) {
    let mut captures = cursor.captures(query, root, content_bytes);

    while let Some((m, capture_index)) = captures.next() {
        let capture = m.captures[*capture_index];
        let capture_name = query.capture_names()[capture.index as usize];
        let node = capture.node;

        match capture_name {
            "call_name" => collect_call(node, content_bytes, calls),

            "class_name" | "struct_name" | "enum_name" => {
                collect_class_like(node, capture_name, content_bytes, classes);
            }

            "base_class_name" => {
                collect_base_class(node, content_bytes, classes);
            }

            "func_node" | "decl_node" | "ufunc_node" | "field_node" => {
                if let Some(member) = build_member(node, capture_name, content_bytes, classes) {
                    pending_members.push((member, node.start_byte(), node.end_byte()));
                }
            }

            "enum_val_name" => {
                let member = MemberInfo {
                    name: get_node_text(&node, content_bytes).to_string(),
                    mem_type: "enum_item".to_string(),
                    flags: String::new(),
                    access: "public".to_string(),
                    line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    detail: None,
                    return_type: None,
                };

                pending_members.push((member, node.start_byte(), node.end_byte()));
            }

            _ => {}
        }
    }
}

/// Record one function or method call.
/// 记录一次函数或方法调用。
fn collect_call(
    node: Node,
    content_bytes: &[u8],
    calls: &mut Vec<crate::types::CallInfo>,
) {
    let name = get_node_text(&node, content_bytes).to_string();

    if !name.is_empty() {
        calls.push(crate::types::CallInfo {
            name,
            line: node.start_position().row + 1,
        });
    }
}

/// Build class / struct / enum records.
/// 构建 class / struct / enum 记录。
fn collect_class_like(
    node: Node,
    capture_name: &str,
    content_bytes: &[u8],
    classes: &mut Vec<ClassInfo>,
) {
    let Some(parent) = node.parent() else {
        return;
    };

    if parent.child_by_field_name("body").is_none() {
        return;
    }

    let mut name = get_node_text(&node, content_bytes).to_string();
    let namespace = get_namespace(&parent, content_bytes);

    if capture_name == "enum_name" && name == "Type" {
        if let Some(ns) = &namespace {
            name = format!("{}::{}", ns, name);
        }
    }

    let symbol_type = match parent.kind() {
        "unreal_reflected_class_declaration" => "UCLASS",
        "unreal_reflected_struct_declaration" => "USTRUCT",
        "unreal_reflected_enum_declaration" => "UENUM",
        _ if capture_name == "struct_name" => "struct",
        _ if capture_name == "enum_name" => "enum",
        _ => "class",
    };

    classes.push(ClassInfo {
        class_name: name,
        namespace,
        base_classes: Vec::new(),
        symbol_type: symbol_type.to_string(),
        line: parent.start_position().row + 1,
        end_line: parent.end_position().row + 1,
        range_start: parent.start_byte(),
        range_end: parent.end_byte(),
        members: Vec::new(),
        is_final: node_has_token(parent, content_bytes, "final"),
        is_interface: node_has_child_kind(parent, "unreal_interface_macro"),
    });
}

/// Attach a base class to the current class.
/// 给当前 class 挂父类。
fn collect_base_class(
    node: Node,
    content_bytes: &[u8],
    classes: &mut [ClassInfo],
) {
    let node_start = node.start_byte();

    let Some(cls) = classes.last_mut() else {
        return;
    };

    if node_start < cls.range_start || node_start > cls.range_end {
        return;
    }

    let mut name = get_node_text(&node, content_bytes).to_string();

    if let Some(idx) = name.rfind("::") {
        name = name[idx + 2..].to_string();
    }

    if !name.is_empty() && name != cls.class_name {
        cls.base_classes.push(name);
    }
}

/// Convert a function/declaration/field node into MemberInfo.
/// 把函数、声明、字段节点转换成 MemberInfo。
fn build_member(
    node: Node,
    capture_name: &str,
    content_bytes: &[u8],
    classes: &mut Vec<ClassInfo>,
) -> Option<MemberInfo> {
    let declarator = find_declarator_node(node)?;
    let member_identity = resolve_member_identity(declarator, content_bytes)?;

    let mut is_function = matches!(capture_name, "func_node" | "ufunc_node")
        || node.kind() == "unreal_function_declaration"
        || member_identity.is_function;

    let mut flags = Vec::new();

    // Node names intentionally match UTreeSitter.
    // 这里的节点名要和 UTreeSitter grammar 保持一致。
    if node_has_child_kind(node, "unreal_function_macro")
        || node_has_child_kind(node, "unreal_function_declaration")
        || node.kind() == "unreal_function_declaration"
    {
        flags.push("UFUNCTION");
        is_function = true;
    }

    if node_has_child_kind(node, "unreal_property_macro") {
        flags.push("UPROPERTY");
        is_function = false;
    }

    let scope_name = member_identity.scope_name;
    let access = if scope_name.is_some() && is_function {
        "impl".to_string()
    } else {
        infer_access(node, content_bytes)
    };

    let return_type = extract_return_or_property_type(node, declarator, content_bytes);

    let detail = if is_function {
        find_child_by_type(node, "parameter_list")
            .map(|params| get_node_text(&params, content_bytes).to_string())
    } else {
        None
    };

    let member_name = member_identity.name;

    if should_skip_member_name(&member_name) {
        return None;
    }

    let mut member = MemberInfo {
        name: member_name,
        mem_type: if is_function { "function" } else { "property" }.to_string(),
        flags: flags.join(" "),
        access,
        line: member_identity.line,
        end_line: node.end_position().row + 1,
        detail,
        return_type,
    };

    // Out-of-class implementation, e.g. UMyWidget::InitInfo.
    // 类外函数实现，例如 UMyWidget::InitInfo。
    if let Some(scope) = scope_name {
        let class_index = find_or_create_impl_class(classes, &scope);
        member.access = "impl".to_string();
        classes[class_index].members.push(member);
        return None;
    }

    Some(member)
}

/// Resolved declarator identity.
/// 从 declarator 里解析出来的成员身份。
struct MemberIdentity {
    name: String,
    scope_name: Option<String>,
    is_function: bool,
    line: usize,
}

/// Walk through nested declarators to find the real member name.
/// 穿过嵌套 declarator，找到真正的成员名。
fn resolve_member_identity(
    declarator: Node,
    content_bytes: &[u8],
) -> Option<MemberIdentity> {
    let mut current = declarator;
    let mut is_function = false;

    loop {
        match current.kind() {
            "identifier" | "field_identifier" => {
                return Some(MemberIdentity {
                    name: get_node_text(&current, content_bytes).to_string(),
                    scope_name: None,
                    is_function,
                    line: current.start_position().row + 1,
                });
            }

            "qualified_identifier" => {
                let scope_name = current
                    .child_by_field_name("scope")
                    .map(|scope| get_node_text(&scope, content_bytes).to_string());

                let name_node = current.child_by_field_name("name")?;
                let name = get_node_text(&name_node, content_bytes).to_string();

                if name.is_empty() {
                    return None;
                }

                return Some(MemberIdentity {
                    name,
                    scope_name,
                    is_function,
                    line: name_node.start_position().row + 1,
                });
            }

            "function_declarator" => {
                is_function = true;

                if let Some(next) = current.child_by_field_name("declarator") {
                    current = next;
                    continue;
                }

                return None;
            }

            "pointer_declarator"
            | "reference_declarator"
            | "array_declarator"
            | "parenthesized_declarator" => {
                if let Some(next) = current.child_by_field_name("declarator") {
                    current = next;
                    continue;
                }

                return None;
            }

            _ => return None,
        }
    }
}

/// Infer public/protected/private from preceding access specifiers.
/// 根据前面的 access specifier 推断 public/protected/private。
fn infer_access(node: Node, content_bytes: &[u8]) -> String {
    let mut access = "public".to_string();
    let mut current = node;

    while let Some(parent) = current.parent() {
        if matches!(
            parent.kind(),
            "field_declaration_list" | "class_specifier" | "struct_specifier"
        ) {
            let mut cursor = parent.walk();

            for child in parent.children(&mut cursor) {
                if child.start_byte() >= current.start_byte() {
                    break;
                }

                if child.kind() == "access_specifier" {
                    access = get_node_text(&child, content_bytes)
                        .trim()
                        .trim_end_matches(':')
                        .to_ascii_lowercase();
                }
            }

            break;
        }

        current = parent;
    }

    access
}

/// Extract return type or property type from text before the declarator.
/// 从 declarator 前面的文本提取返回类型或属性类型。
fn extract_return_or_property_type(
    node: Node,
    declarator: Node,
    content_bytes: &[u8],
) -> Option<String> {
    let start = node.start_byte();
    let end = declarator.start_byte();

    if end <= start {
        return None;
    }

    let mut prefix = &content_bytes[start..end];

    // Skip macro argument text before the real type.
    // 跳过真正类型前面的宏参数文本。
    if let Some(idx) = prefix.iter().rposition(|&b| b == b')') {
        prefix = &prefix[idx + 1..];
    }

    let raw = std::str::from_utf8(prefix).unwrap_or("");
    let cleaned = clean_type_string(raw);

    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Attach collected members to their smallest containing class.
/// 把收集到的成员挂到最小的包含它的 class 上。
fn attach_pending_members(
    classes: &mut [ClassInfo],
    pending_members: Vec<(MemberInfo, usize, usize)>,
) {
    for (member, member_start, member_end) in pending_members {
        let best_class = classes
            .iter()
            .enumerate()
            .filter(|(_, class_info)| {
                member_start >= class_info.range_start && member_end <= class_info.range_end
            })
            .min_by_key(|(_, class_info)| class_info.range_end - class_info.range_start)
            .map(|(index, _)| index);

        if let Some(index) = best_class {
            classes[index].members.push(member);
        }
    }
}

/// Get or create a synthetic class record for implementation-only files.
/// 获取或创建只在 cpp 实现文件里出现的虚拟 class 记录。
fn find_or_create_impl_class(classes: &mut Vec<ClassInfo>, scope: &str) -> usize {
    if let Some(index) = classes.iter().position(|class_info| class_info.class_name == scope) {
        return index;
    }

    classes.push(ClassInfo {
        class_name: scope.to_string(),
        namespace: None,
        base_classes: Vec::new(),
        symbol_type: "class".to_string(),
        line: 1,
        end_line: usize::MAX,
        range_start: 0,
        range_end: usize::MAX,
        members: Vec::new(),
        is_final: false,
        is_interface: false,
    });

    classes.len() - 1
}

fn should_skip_member_name(name: &str) -> bool {
    matches!(name, "virtual" | "static" | "void" | "const" | "class" | "struct")
}

fn get_node_text<'a>(node: &Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Build namespace path from parent namespace/class/struct nodes.
/// 从父级 namespace/class/struct 节点构造命名空间路径。
fn get_namespace<'a>(node: &Node<'a>, source: &'a [u8]) -> Option<String> {
    let mut parts = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "namespace_definition" | "class_specifier" | "struct_specifier"
        ) {
            if let Some(name) = parent.child_by_field_name("name") {
                parts.push(get_node_text(&name, source).to_string());
            }
        }

        current = parent.parent();
    }

    if parts.is_empty() {
        None
    } else {
        parts.reverse();
        Some(parts.join("::"))
    }
}

/// Depth-first search for a child node kind.
/// 深度优先查找指定 kind 的子节点。
fn find_child_by_type<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }

        if let Some(found) = find_child_by_type(child, kind) {
            return Some(found);
        }
    }

    None
}

/// Recursively check whether a node contains a child kind.
/// 递归检查节点是否包含某种子节点。
fn node_has_child_kind(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        if child.kind() == kind || node_has_child_kind(child, kind) {
            return true;
        }
    }

    false
}

/// Check whether a token exists in node text.
/// 检查节点文本中是否包含某个 token。
fn node_has_token(node: Node, content_bytes: &[u8], token: &str) -> bool {
    get_node_text(&node, content_bytes)
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|part| part == token)
}

/// Find the first nested declarator field.
/// 查找第一个嵌套的 declarator 字段。
fn find_declarator_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    for index in 0..node.child_count() {
        if node.field_name_for_child(index as u32) == Some("declarator") {
            return node.child(index as u32);
        }

        if let Some(child) = node.child(index as u32) {
            if let Some(found) = find_declarator_node(child) {
                return Some(found);
            }
        }
    }

    None
}

/// Clean C++ type text down to the meaningful type name.
/// 把 C++ 类型文本清理成真正有意义的类型名。
fn clean_type_string(raw: &str) -> String {
    let regexes = get_clean_regexes();

    let mut clean = raw.trim().to_string();

    for keyword in &regexes.keywords {
        clean = keyword.replace_all(&clean, "").to_string();
    }

    clean = regexes.api.replace_all(&clean, "").to_string();
    clean = regexes.unreal_macros.replace_all(&clean, "").to_string();
    clean = clean.replace(';', "");
    clean = clean.replace(':', " : ");
    clean = regexes.whitespace.replace_all(&clean, " ").to_string();
    clean = clean.trim().to_string();

    if clean.contains('<') && clean.contains('>') {
        return clean;
    }

    clean
        .split_whitespace()
        .last()
        .unwrap_or("")
        .to_string()
}
