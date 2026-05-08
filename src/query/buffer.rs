use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tree_sitter::{Node, Parser, Point, Query, QueryCursor};
use streaming_iterator::StreamingIterator;

/// Parse an in-memory Unreal C++ buffer.
/// 解析内存中的 Unreal C++ buffer。
pub fn parse_buffer(
    content: String,
    file_path: Option<String>,
    line: Option<u32>,
    character: Option<u32>,
) -> Result<Value> {
    let path = normalize_path(file_path.unwrap_or_else(|| "buffer.cpp".to_string()));
    let language: tree_sitter::Language = tree_sitter_unreal_cpp::LANGUAGE.into();

    let query = Query::new(&language, crate::scanner::QUERY_STR)
        .context("failed to compile scanner query")?;

    let (classes, _, _) = crate::scanner::parse_content(&content, &path, &language, &query)
        .context("failed to parse buffer symbols")?;

    let symbols = classes
        .into_iter()
        .map(|class_info| class_to_json(class_info, &path))
        .collect::<Vec<_>>();

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("failed to set unreal_cpp language")?;

    let tree = parser
        .parse(&content, None)
        .ok_or_else(|| anyhow!("failed to parse buffer"))?;
    let root = tree.root_node();

    let include_info = collect_include_info(root, &content, &language)?;
    let cursor_info = match (line, character) {
        (Some(row), Some(col)) => analyze_cursor(root, &content, row as usize, col as usize),
        _ => Value::Null,
    };

    Ok(json!({
        "symbols": symbols,
        "cursor_info": cursor_info,
        "metadata": {
            "generated_h_line": include_info.generated_h_line,
            "last_include_line": include_info.last_include_line,
            "suggested_insert_line": include_info.suggested_insert_line,
            "includes": include_info.includes,
        }
    }))
}

/// Include metadata for one buffer.
/// 单个 buffer 的 include 元数据。
struct IncludeInfo {
    generated_h_line: usize,
    last_include_line: usize,
    suggested_insert_line: usize,
    includes: Vec<Value>,
}

/// Convert class info into the JSON shape expected by Lua.
/// 把 class info 转成 Lua 侧需要的 JSON 结构。
fn class_to_json(class_info: crate::types::ClassInfo, path: &str) -> Value {
    let mut class_json = json!({
        "name": class_info.class_name,
        "kind": class_info.symbol_type,
        "line": class_info.line,
        "end_line": class_info.end_line,
        "namespace": class_info.namespace,
        "base_class": class_info.base_classes.first(),
        "file_path": path,
        "fields": {
            "public": [],
            "protected": [],
            "private": [],
            "impl": [],
        },
        "methods": {
            "public": [],
            "protected": [],
            "private": [],
            "impl": [],
        },
    });

    for member in class_info.members {
        let access = normalize_access(&member.access);
        let bucket = if member.mem_type.to_ascii_lowercase().contains("function") {
            "methods"
        } else {
            "fields"
        };

        let member_json = json!({
            "name": member.name,
            "kind": member.mem_type,
            "flags": member.flags,
            "access": member.access,
            "detail": member.detail,
            "return_type": member.return_type,
            "file_path": path,
            "line": member.line,
            "end_line": member.end_line,
        });

        class_json[bucket][access]
            .as_array_mut()
            .expect("class member bucket must be an array")
            .push(member_json);
    }

    class_json
}

/// Collect include lines and suggested insertion point.
/// 收集 include 行信息和建议插入位置。
fn collect_include_info(root: Node, content: &str, language: &tree_sitter::Language) -> Result<IncludeInfo> {
    let query = Query::new(
        language,
        r#"
        (preproc_include
          path: [
            (string_literal) @path
            (system_lib_string) @path
          ]) @include
        "#,
    )
    .context("failed to compile include query")?;

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, content.as_bytes());

    let mut generated_h_line = 0usize;
    let mut last_include_line = 0usize;
    let mut includes = Vec::new();

    let cap_names = query.capture_names();
    let path_cap = cap_index(&cap_names, "path");
    let include_cap = cap_index(&cap_names, "include");

    while let Some(m) = matches.next() {
        let mut include_node = None;
        let mut include_path = String::new();

        for capture in m.captures {
            if Some(capture.index) == path_cap {
                include_path = clean_include_path(node_text(capture.node, content));
            }

            if Some(capture.index) == include_cap {
                include_node = Some(capture.node);
            }
        }

        let Some(node) = include_node else {
            continue;
        };

        let line = node.start_position().row + 1;
        let is_generated = include_path.contains(".generated.h");

        last_include_line = line;

        if is_generated {
            generated_h_line = line;
        }

        includes.push(json!({
            "line": line,
            "path": include_path,
            "is_generated": is_generated,
        }));
    }

    let suggested_insert_line = suggest_include_insert_line(content, generated_h_line, last_include_line);

    Ok(IncludeInfo {
        generated_h_line,
        last_include_line,
        suggested_insert_line,
        includes,
    })
}

/// Suggest where a new include should be inserted.
/// 建议新 include 应该插入到哪一行。
fn suggest_include_insert_line(
    content: &str,
    generated_h_line: usize,
    last_include_line: usize,
) -> usize {
    if last_include_line > 0 {
        let mut line = last_include_line + 1;

        // Unreal requires `.generated.h` to stay last, so insert before it.
        // Unreal 要求 `.generated.h` 保持最后，所以插入到它前面。
        if generated_h_line > 0 && line > generated_h_line {
            line = generated_h_line;
        }

        return line;
    }

    for (index, text) in content.lines().enumerate() {
        if text.contains("#pragma once") {
            return index + 2;
        }
    }

    1
}

/// Analyze node under cursor.
/// 分析光标下的语法节点。
fn analyze_cursor(root: Node, content: &str, line: usize, character: usize) -> Value {
    let point = Point::new(line, character);

    let Some(node) = root.descendant_for_point_range(point, point) else {
        return Value::Null;
    };

    analyze_cursor_node(node, content)
}

/// Walk upward from cursor node and return enclosing declaration info.
/// 从光标节点向上查找，并返回所在声明信息。
fn analyze_cursor_node(node: Node, content: &str) -> Value {
    let mut current = Some(node);

    while let Some(node) = current {
        if is_function_or_field_node(node) {
            return declaration_node_to_json(node, content);
        }

        current = node.parent();
    }

    Value::Null
}

/// Convert one function/field declaration node into JSON.
/// 把函数或字段声明节点转成 JSON。
fn declaration_node_to_json(node: Node, content: &str) -> Value {
    let text = node_text(node, content);
    let declarator = find_child_by_field(node, "declarator");

    let name = declarator
        .and_then(find_name_node)
        .map(|name_node| node_text(name_node, content).to_string())
        .unwrap_or_default();

    let parameters = declarator
        .and_then(|decl| find_child_by_type(decl, "parameter_list"))
        .map(|params| node_text(params, content).to_string())
        .unwrap_or_default();

    let return_type = find_child_by_field(node, "type")
        .map(|type_node| node_text(type_node, content).to_string())
        .unwrap_or_else(|| extract_prefix_type(node, declarator, content));

    let class_name = find_enclosing_class_name(node, content).unwrap_or_default();
    let generated_definitions = generated_definitions_json(&name, &return_type, text);

    json!({
        "kind": node.kind(),
        "name": name,
        "class_name": class_name,
        "return_type": return_type,
        "parameters": parameters,
        "is_virtual": contains_token(text, "virtual"),
        "is_static": contains_token(text, "static"),
        "is_const": is_const_member_function(node, content),
        "full_text": text,
        "generated_definitions": generated_definitions,
    })
}

fn generated_definitions_json(name: &str, return_type: &str, full_text: &str) -> Vec<Value> {
    let spec = parse_ufunction_spec(full_text);
    let base_name = base_unreal_function_name(name);
    let implementation_name = if spec.requires_implementation {
        format!("{}_Implementation", base_name)
    } else {
        name.to_string()
    };

    let mut items = vec![json!({
        "name": implementation_name,
        "return_type": return_type.trim(),
        "kind": if spec.requires_implementation { "implementation" } else { "definition" },
    })];

    if spec.requires_validate {
        items.push(json!({
            "name": format!("{}_Validate", base_name),
            "return_type": "bool",
            "kind": "validation",
        }));
    }

    items
}

#[derive(Default)]
struct UnrealFunctionSpec {
    requires_implementation: bool,
    requires_validate: bool,
}

fn base_unreal_function_name(name: &str) -> &str {
    if let Some(stripped) = name.strip_suffix("_Implementation") {
        return stripped;
    }

    if let Some(stripped) = name.strip_suffix("_Validate") {
        return stripped;
    }

    name
}

fn parse_ufunction_spec(text: &str) -> UnrealFunctionSpec {
    let Some(specifiers) = extract_macro_arguments(text, "UFUNCTION") else {
        return UnrealFunctionSpec::default();
    };

    let has_token = |token: &str| contains_token(&specifiers, token);

    UnrealFunctionSpec {
        requires_implementation: has_token("BlueprintNativeEvent")
            || has_token("Server")
            || has_token("Client")
            || has_token("NetMulticast"),
        requires_validate: has_token("WithValidation"),
    }
}

fn extract_macro_arguments(text: &str, macro_name: &str) -> Option<String> {
    let start = text.find(macro_name)?;
    let after_name = text.get(start + macro_name.len()..)?;
    let open_offset = after_name.find('(')?;
    let open_index = start + macro_name.len() + open_offset;
    let mut depth = 0i32;

    for (offset, ch) in text[open_index..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let begin = open_index + 1;
                    let end = open_index + offset;
                    return text.get(begin..end).map(|value| value.to_string());
                }
            }
            _ => {}
        }
    }

    None
}

/// Return true for declaration nodes we care about.
/// 判断是否是需要分析的声明节点。
fn is_function_or_field_node(node: Node) -> bool {
    matches!(
        node.kind(),
        "field_declaration" | "declaration" | "function_definition" | "unreal_function_declaration"
    )
}

/// Find the real name node through nested declarators.
/// 穿过嵌套 declarator 查找真实名称节点。
fn find_name_node(node: Node) -> Option<Node> {
    match node.kind() {
        "identifier" | "field_identifier" => Some(node),

        "qualified_identifier" => node.child_by_field_name("name"),

        "function_declarator"
        | "pointer_declarator"
        | "reference_declarator"
        | "array_declarator"
        | "parenthesized_declarator" => node
            .child_by_field_name("declarator")
            .and_then(find_name_node),

        _ => {
            let mut cursor = node.walk();

            for child in node.children(&mut cursor) {
                if let Some(found) = find_name_node(child) {
                    return Some(found);
                }
            }

            None
        }
    }
}

/// Find a direct child by field name.
/// 按字段名查找直接子节点。
fn find_child_by_field<'a>(node: Node<'a>, field: &str) -> Option<Node<'a>> {
    node.child_by_field_name(field)
}

/// Recursively find a child by node kind.
/// 递归查找指定 kind 的子节点。
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

/// Find enclosing Unreal/C++ class or struct name.
/// 查找外层 Unreal/C++ class 或 struct 名称。
fn find_enclosing_class_name(node: Node, content: &str) -> Option<String> {
    let mut current = node.parent();

    while let Some(node) = current {
        if matches!(
            node.kind(),
            "class_specifier"
                | "struct_specifier"
                | "unreal_reflected_class_declaration"
                | "unreal_reflected_struct_declaration"
        ) {
            if let Some(name_node) = node.child_by_field_name("name") {
                return Some(node_text(name_node, content).trim().to_string());
            }
        }

        current = node.parent();
    }

    None
}

/// Extract a type from text before the declarator when no `type` field exists.
/// 当没有 `type` 字段时，从 declarator 前面的文本里提取类型。
fn extract_prefix_type(node: Node, declarator: Option<Node>, content: &str) -> String {
    let Some(declarator) = declarator else {
        return String::new();
    };

    let start = node.start_byte();
    let end = declarator.start_byte();

    if end <= start || end > content.len() {
        return String::new();
    }

    let prefix = &content[start..end];

    prefix
        .split_whitespace()
        .last()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Detect const member functions, not just const parameters.
/// 检测 const 成员函数，而不是参数里的 const。
fn is_const_member_function(node: Node, content: &str) -> bool {
    let Some(declarator) = find_child_by_field(node, "declarator") else {
        return false;
    };

    let Some(params) = find_child_by_type(declarator, "parameter_list") else {
        return false;
    };

    let after_params_start = params.end_byte();
    let node_end = node.end_byte();

    if after_params_start >= node_end || node_end > content.len() {
        return false;
    }

    contains_token(&content[after_params_start..node_end], "const")
}

/// Test whether text contains an exact identifier token.
/// 判断文本是否包含完整 token。
fn contains_token(text: &str, token: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .any(|part| part == token)
}

/// Get text of one tree-sitter node.
/// 获取 tree-sitter 节点文本。
fn node_text<'a>(node: Node, content: &'a str) -> &'a str {
    let range = node.byte_range();

    if range.end <= content.len()
        && content.is_char_boundary(range.start)
        && content.is_char_boundary(range.end)
    {
        &content[range.start..range.end]
    } else {
        ""
    }
}

/// Get capture index by name.
/// 根据 capture 名获取 capture index。
fn cap_index(names: &[&str], target: &str) -> Option<u32> {
    names
        .iter()
        .position(|name| *name == target)
        .map(|index| index as u32)
}

/// Clean include path token.
/// 清理 include path token。
fn clean_include_path(path: &str) -> String {
    path.trim()
        .trim_matches('"')
        .trim_matches('<')
        .trim_matches('>')
        .to_string()
}

/// Normalize paths for JSON output.
/// 规范化 JSON 输出里的路径。
fn normalize_path(path: String) -> String {
    if std::path::MAIN_SEPARATOR == '\\' {
        path.replace('\\', "/")
    } else {
        path
    }
}

/// Normalize access key for JSON buckets.
/// 规范化 JSON 分组里的 access key。
fn normalize_access(access: &str) -> &str {
    match access.to_ascii_lowercase().as_str() {
        "public" => "public",
        "protected" => "protected",
        "private" => "private",
        "impl" => "impl",
        _ => "public",
    }
}
