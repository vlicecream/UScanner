use anyhow::Result;
use regex::Regex;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use tree_sitter::Parser;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiagnosticItem {
    pub file_path: Option<String>,
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub severity: DiagnosticSeverity,
    pub source: &'static str,
    pub code: &'static str,
    pub message: String,
}

impl DiagnosticItem {
    fn new(
        file_path: Option<&str>,
        line: u32,
        character: u32,
        severity: DiagnosticSeverity,
        source: &'static str,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            file_path: file_path.map(|path| path.replace('\\', "/")),
            line,
            character,
            end_line: line,
            end_character: character.saturating_add(1),
            severity,
            source,
            code,
            message: message.into(),
        }
    }

    fn with_end(mut self, end_line: u32, end_character: u32) -> Self {
        self.end_line = end_line;
        self.end_character = end_character.max(self.character.saturating_add(1));
        self
    }
}

pub fn process_diagnostics(
    _conn: &Connection,
    _engine_conn: Option<&Connection>,
    content: &str,
    file_path: Option<String>,
) -> Result<Value> {
    let mut items = Vec::new();
    items.extend(unreal_rule_diagnostics(content, file_path.as_deref())?);
    items.extend(missing_implementation_diagnostics(content, file_path.as_deref())?);
    Ok(json!({ "items": items }))
}

pub fn parse_build_diagnostics(output: &str) -> Value {
    json!({ "items": build_log_diagnostics(output) })
}

fn unreal_rule_diagnostics(content: &str, file_path: Option<&str>) -> Result<Vec<DiagnosticItem>> {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_unreal_cpp::LANGUAGE.into();
    parser.set_language(&language)?;
    let _tree = parser.parse(content, None);

    let mut items = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();

        if starts_unreal_type_macro(trimmed) {
            let (macro_text, macro_end) = macro_invocation_text(&lines, index);

            if let Some((next_index, next_line)) = next_meaningful_line(&lines, macro_end + 1) {
                if !macro_matches_declaration(trimmed, next_line.trim_start()) {
                    items.push(DiagnosticItem::new(
                        file_path,
                        index as u32,
                        leading_spaces(line) as u32,
                        DiagnosticSeverity::Error,
                        "UCore",
                        "UHT001",
                        "Unreal reflection macro does not match the following declaration.",
                    )
                    .with_end(index as u32, line.len() as u32));
                }

                if !macro_text.starts_with("UENUM")
                    && !declaration_block_has_generated_body(&lines, next_index)
                {
                    items.push(DiagnosticItem::new(
                        file_path,
                        next_index as u32,
                        leading_spaces(next_line) as u32,
                        DiagnosticSeverity::Error,
                        "UCore",
                        "UHT002",
                        "Reflected type is missing GENERATED_BODY().",
                    )
                    .with_end(next_index as u32, next_line.len() as u32));
                }
            }
        }

        if trimmed.starts_with("UFUNCTION(") {
            let (macro_text, _) = macro_invocation_text(&lines, index);
            if macro_text.contains("BlueprintCallable") && !macro_text.contains("Category") {
                items.push(DiagnosticItem::new(
                    file_path,
                    index as u32,
                    leading_spaces(line) as u32,
                    DiagnosticSeverity::Hint,
                    "UCore",
                    "UEBP001",
                    "BlueprintCallable functions should declare a Category.",
                )
                .with_end(index as u32, line.len() as u32));
            }
        }

        if trimmed.starts_with("UPROPERTY(")
        {
            let (macro_text, _) = macro_invocation_text(&lines, index);
            if macro_text.contains("BlueprintReadWrite")
                && !macro_text.contains("AllowPrivateAccess")
                && nearest_access_section(&lines, index) == Some("private")
            {
                items.push(DiagnosticItem::new(
                    file_path,
                    index as u32,
                    leading_spaces(line) as u32,
                    DiagnosticSeverity::Warning,
                    "UCore",
                    "UEBP002",
                    "Private BlueprintReadWrite property should use meta=(AllowPrivateAccess=true).",
                )
                .with_end(index as u32, line.len() as u32));
            }
        }
    }

    Ok(items)
}

fn missing_implementation_diagnostics(
    content: &str,
    file_path: Option<&str>,
) -> Result<Vec<DiagnosticItem>> {
    let Some(header_path) = file_path else {
        return Ok(Vec::new());
    };

    if !is_header_file(header_path) {
        return Ok(Vec::new());
    }

    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_unreal_cpp::LANGUAGE.into();
    parser.set_language(&language)?;

    let Some(tree) = parser.parse(content, None) else {
        return Ok(Vec::new());
    };

    let root = tree.root_node();
    let source_candidates = header_to_source_candidates(header_path);
    let source_texts = source_candidates
        .iter()
        .filter_map(|path| {
            fs::read_to_string(path)
                .ok()
                .map(|text| (path.replace('\\', "/"), normalize_space(&text)))
        })
        .collect::<Vec<_>>();

    let mut items = Vec::new();
    collect_missing_impl_items(
        root,
        content,
        header_path,
        &source_candidates,
        &source_texts,
        &mut items,
    );
    Ok(items)
}

fn collect_missing_impl_items(
    node: tree_sitter::Node,
    content: &str,
    file_path: &str,
    source_candidates: &[String],
    source_texts: &[(String, String)],
    items: &mut Vec<DiagnosticItem>,
) {
    if let Some(decl) = member_function_declaration(node, content, file_path) {
        let target = source_texts
            .first()
            .map(|(path, _)| path.clone())
            .or_else(|| source_candidates.first().cloned())
            .unwrap_or_else(|| expected_source_path(&decl.class_name));

        for expected in &decl.expected_definitions {
            let definition_signature = build_definition_signature(&decl, expected);
            let found = source_texts
                .iter()
                .any(|(_, text)| has_definition_text(text, &definition_signature));

            if !found {
                items.push(
                    DiagnosticItem::new(
                        decl.file_path.as_deref(),
                        decl.line,
                        decl.character,
                        DiagnosticSeverity::Warning,
                        "UCore",
                        "UECPP001",
                        format!(
                            "No matching .cpp {} found for {}::{}{}. Expected in {}.",
                            expected.message_label,
                            decl.class_name,
                            expected.name,
                            decl.parameters,
                            target
                        ),
                    )
                    .with_end(decl.end_line, decl.end_character),
                );
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_missing_impl_items(child, content, file_path, source_candidates, source_texts, items);
    }
}

#[derive(Clone, Debug)]
struct ExpectedDefinition {
    name: String,
    return_type: String,
    message_label: &'static str,
}

#[derive(Clone, Debug, Default)]
struct UnrealFunctionSpec {
    requires_implementation: bool,
    requires_validate: bool,
}

#[derive(Clone, Debug)]
struct HeaderFunctionDecl {
    file_path: Option<String>,
    line: u32,
    character: u32,
    end_line: u32,
    end_character: u32,
    class_name: String,
    name: String,
    parameters: String,
    return_type: String,
    full_text: String,
    is_const: bool,
    expected_definitions: Vec<ExpectedDefinition>,
}

fn member_function_declaration(
    node: tree_sitter::Node,
    content: &str,
    file_path: &str,
) -> Option<HeaderFunctionDecl> {
    if !matches!(
        node.kind(),
        "field_declaration" | "unreal_function_declaration" | "declaration"
    ) {
        return None;
    }

    if has_enclosing_template(node) {
        return None;
    }

    let text = node_text(node, content).trim().to_string();
    if text.is_empty()
        || text.contains('{')
        || !text.contains(';')
        || contains_token(&text, "inline")
        || contains_token(&text, "FORCEINLINE")
        || contains_token(&text, "friend")
        || text.contains("= 0")
        || text.contains("= delete")
        || text.contains("= default")
    {
        return None;
    }

    let declarator = find_child_by_field(node, "declarator")?;
    let name_node = find_name_node(declarator)?;
    let parameters = find_child_by_type(declarator, "parameter_list")
        .map(|params| node_text(params, content).to_string())
        .unwrap_or_default();

    if parameters.is_empty() {
        return None;
    }

    let class_name = find_enclosing_class_name(node, content)?;
    let name = node_text(name_node, content).trim().to_string();
    if name.is_empty() {
        return None;
    }

    let mut return_type = find_child_by_field(node, "type")
        .map(|type_node| node_text(type_node, content).to_string())
        .unwrap_or_else(|| extract_prefix_type(node, Some(declarator), content));

    if name == class_name || name == format!("~{}", class_name) {
        return_type.clear();
    }

    let start = declaration_start(node, declarator);
    let end = node.end_position();
    let mut decl = HeaderFunctionDecl {
        file_path: Some(file_path.replace('\\', "/")),
        line: start.row as u32,
        character: start.column as u32,
        end_line: end.row as u32,
        end_character: end.column as u32,
        class_name,
        name,
        parameters,
        return_type: return_type.trim().to_string(),
        full_text: text,
        is_const: is_const_member_function(node, content),
        expected_definitions: Vec::new(),
    };
    decl.expected_definitions = expected_definitions(&decl);

    Some(decl)
}

fn build_definition_signature(decl: &HeaderFunctionDecl, expected: &ExpectedDefinition) -> String {
    let normalized_parameters = normalize_parameter_signature(&decl.parameters);
    let mut signature = if expected.return_type.is_empty() {
        format!(
            "{}::{}{}",
            decl.class_name, expected.name, normalized_parameters
        )
    } else {
        format!(
            "{} {}::{}{}",
            expected.return_type, decl.class_name, expected.name, normalized_parameters
        )
    };

    signature.push_str(&definition_suffix(decl));
    signature
}

fn expected_definitions(decl: &HeaderFunctionDecl) -> Vec<ExpectedDefinition> {
    let spec = parse_ufunction_spec(&decl.full_text);
    let base_name = base_unreal_function_name(&decl.name);
    let implementation_name = if spec.requires_implementation {
        format!("{}_Implementation", base_name)
    } else {
        decl.name.clone()
    };

    let mut items = vec![ExpectedDefinition {
        name: implementation_name,
        return_type: decl.return_type.clone(),
        message_label: if spec.requires_implementation {
            "implementation"
        } else {
            "definition"
        },
    }];

    if spec.requires_validate {
        items.push(ExpectedDefinition {
            name: format!("{}_Validate", base_name),
            return_type: "bool".to_string(),
            message_label: "validation function",
        });
    }

    items
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

fn declaration_start(
    node: tree_sitter::Node,
    declarator: tree_sitter::Node,
) -> tree_sitter::Point {
    if let Some(type_node) = find_child_by_field(node, "type") {
        return type_node.start_position();
    }

    let node_start = node.start_position();
    let declarator_start = declarator.start_position();

    if declarator_start.row < node_start.row
        || (declarator_start.row == node_start.row && declarator_start.column < node_start.column)
    {
        declarator_start
    } else {
        node_start
    }
}

fn definition_suffix(decl: &HeaderFunctionDecl) -> String {
    let mut suffixes = Vec::new();
    let params_end = decl
        .full_text
        .find(&decl.parameters)
        .map(|start| start + decl.parameters.len());
    let trailing = params_end
        .and_then(|start| decl.full_text.get(start..))
        .unwrap_or("");

    if decl.is_const {
        suffixes.push("const".to_string());
    }

    if let Some(noexcept_text) = extract_noexcept_text(trailing) {
        suffixes.push(noexcept_text);
    }

    if trailing.contains("&&") {
        suffixes.push("&&".to_string());
    } else if trailing.contains('&') {
        suffixes.push("&".to_string());
    }

    if suffixes.is_empty() {
        String::new()
    } else {
        format!(" {}", suffixes.join(" "))
    }
}

fn extract_noexcept_text(trailing: &str) -> Option<String> {
    let noexcept_index = trailing.find("noexcept")?;
    let rest = trailing.get(noexcept_index..)?.trim_start();

    if let Some(paren_start) = rest.find('(') {
        let mut depth = 0i32;
        for (index, ch) in rest.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 && index >= paren_start {
                        return Some(rest[..=index].trim().to_string());
                    }
                }
                _ => {}
            }
        }
    }

    Some("noexcept".to_string())
}

fn has_definition_text(source_text: &str, signature: &str) -> bool {
    let normalized_signature = normalize_space(signature);
    !normalized_signature.is_empty() && source_text.contains(&normalized_signature)
}

fn normalize_space(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_parameter_signature(params: &str) -> String {
    let mut out = String::with_capacity(params.len());
    let mut paren_depth = 0i32;
    let mut angle_depth = 0i32;
    let mut brace_depth = 0i32;
    let mut bracket_depth = 0i32;
    let mut skipping_default = false;

    for ch in params.chars() {
        if skipping_default {
            match ch {
                '(' => paren_depth += 1,
                ')' => {
                    if paren_depth == 1 && angle_depth == 0 && brace_depth == 0 && bracket_depth == 0 {
                        skipping_default = false;
                        paren_depth -= 1;
                        out.push(')');
                    } else {
                        paren_depth -= 1;
                    }
                }
                '<' => angle_depth += 1,
                '>' => angle_depth = (angle_depth - 1).max(0),
                '{' => brace_depth += 1,
                '}' => brace_depth = (brace_depth - 1).max(0),
                '[' => bracket_depth += 1,
                ']' => bracket_depth = (bracket_depth - 1).max(0),
                ',' if paren_depth == 1 && angle_depth == 0 && brace_depth == 0 && bracket_depth == 0 => {
                    skipping_default = false;
                    out.push(',');
                }
                _ => {}
            }
            continue;
        }

        match ch {
            '(' => {
                paren_depth += 1;
                out.push(ch);
            }
            ')' => {
                paren_depth -= 1;
                out.push(ch);
            }
            '<' => {
                angle_depth += 1;
                out.push(ch);
            }
            '>' => {
                angle_depth = (angle_depth - 1).max(0);
                out.push(ch);
            }
            '{' => {
                brace_depth += 1;
                out.push(ch);
            }
            '}' => {
                brace_depth = (brace_depth - 1).max(0);
                out.push(ch);
            }
            '[' => {
                bracket_depth += 1;
                out.push(ch);
            }
            ']' => {
                bracket_depth = (bracket_depth - 1).max(0);
                out.push(ch);
            }
            '=' if paren_depth >= 1 && angle_depth == 0 && brace_depth == 0 && bracket_depth == 0 => {
                skipping_default = true;
            }
            _ => out.push(ch),
        }
    }

    normalize_space(&out)
        .replace(" )", ")")
        .replace(" ,", ",")
}

fn is_header_file(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("h" | "hpp" | "hh" | "hxx")
    )
}

fn header_to_source_candidates(path: &str) -> Vec<String> {
    let normalized = path.replace('\\', "/");
    let Some(dot) = normalized.rfind('.') else {
        return Vec::new();
    };

    let base = &normalized[..dot];
    let mut candidates = Vec::new();

    for ext in [".cpp", ".cc", ".cxx"] {
        candidates.push(format!("{base}{ext}"));
    }

    let mapped = normalized
        .replace("/Classes/", "/Private/")
        .replace("/Public/", "/Private/");
    if mapped != normalized {
        let Some(mapped_dot) = mapped.rfind('.') else {
            return candidates;
        };
        let mapped_base = &mapped[..mapped_dot];
        for ext in [".cpp", ".cc", ".cxx"] {
            let candidate = format!("{mapped_base}{ext}");
            if !candidates.contains(&candidate) {
                candidates.insert(0, candidate);
            }
        }
    }

    candidates
}

fn expected_source_path(class_name: &str) -> String {
    format!("{class_name}.cpp")
}

fn has_enclosing_template(node: tree_sitter::Node) -> bool {
    let mut current = node.parent();

    while let Some(parent) = current {
        if parent.kind() == "template_declaration" {
            return true;
        }
        current = parent.parent();
    }

    false
}

fn find_child_by_field<'a>(node: tree_sitter::Node<'a>, field: &str) -> Option<tree_sitter::Node<'a>> {
    node.child_by_field_name(field)
}

fn find_child_by_type<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
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

fn find_name_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
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

fn find_enclosing_class_name(node: tree_sitter::Node, content: &str) -> Option<String> {
    let mut current = node.parent();

    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "class_specifier"
                | "struct_specifier"
                | "unreal_reflected_class_declaration"
                | "unreal_reflected_struct_declaration"
        ) {
            if let Some(name_node) = parent.child_by_field_name("name") {
                return Some(node_text(name_node, content).trim().to_string());
            }
        }

        current = parent.parent();
    }

    None
}

fn extract_prefix_type(
    node: tree_sitter::Node,
    declarator: Option<tree_sitter::Node>,
    content: &str,
) -> String {
    let Some(declarator) = declarator else {
        return String::new();
    };

    let start = node.start_byte();
    let end = declarator.start_byte();

    if end <= start || end > content.len() {
        return String::new();
    }

    content[start..end]
        .split_whitespace()
        .last()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn is_const_member_function(node: tree_sitter::Node, content: &str) -> bool {
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

fn contains_token(text: &str, token: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .any(|part| part == token)
}

fn node_text<'a>(node: tree_sitter::Node, content: &'a str) -> &'a str {
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

fn build_log_diagnostics(output: &str) -> Vec<DiagnosticItem> {
    let msvc = Regex::new(
        r#"(?m)^(?P<file>[A-Za-z]:[^\r\n()]+)\((?P<line>\d+)(?:,(?P<col>\d+))?\):\s*(?P<level>fatal error|error|warning)\s*(?P<code>[A-Z]+\d+):\s*(?P<msg>.+)$"#,
    )
    .unwrap();
    let uht = Regex::new(
        r#"(?m)^(?P<file>[A-Za-z]:[^\r\n:]+):(?P<line>\d+):\s*(?P<level>Error|Warning):\s*(?P<msg>.+)$"#,
    )
    .unwrap();

    let mut items = Vec::new();

    for cap in msvc.captures_iter(output) {
        let level = cap.name("level").map(|m| m.as_str()).unwrap_or("error");
        let severity = if level.contains("warning") {
            DiagnosticSeverity::Warning
        } else {
            DiagnosticSeverity::Error
        };
        let line = cap
            .name("line")
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(1)
            .saturating_sub(1);
        let col = cap
            .name("col")
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(1)
            .saturating_sub(1);
        let code = cap.name("code").map(|m| m.as_str()).unwrap_or("MSVC");
        let msg = cap.name("msg").map(|m| m.as_str()).unwrap_or("");

        items.push(DiagnosticItem::new(
            cap.name("file").map(|m| m.as_str()),
            line,
            col,
            severity,
            "MSVC",
            "BUILD",
            format!("{}: {}", code, msg),
        ));
    }

    for cap in uht.captures_iter(output) {
        let level = cap.name("level").map(|m| m.as_str()).unwrap_or("Error");
        let severity = if level.eq_ignore_ascii_case("warning") {
            DiagnosticSeverity::Warning
        } else {
            DiagnosticSeverity::Error
        };
        let line = cap
            .name("line")
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(1)
            .saturating_sub(1);
        let msg = cap.name("msg").map(|m| m.as_str()).unwrap_or("");

        items.push(DiagnosticItem::new(
            cap.name("file").map(|m| m.as_str()),
            line,
            0,
            severity,
            "UHT",
            "BUILD",
            msg,
        ));
    }

    items
}

fn starts_unreal_type_macro(text: &str) -> bool {
    text.starts_with("UCLASS(")
        || text.starts_with("USTRUCT(")
        || text.starts_with("UENUM(")
        || text == "UCLASS"
        || text == "USTRUCT"
        || text == "UENUM"
}

fn macro_matches_declaration(macro_line: &str, declaration: &str) -> bool {
    if macro_line.starts_with("UCLASS") {
        declaration.contains("class ")
    } else if macro_line.starts_with("USTRUCT") {
        declaration.contains("struct ")
    } else if macro_line.starts_with("UENUM") {
        declaration.contains("enum ")
    } else {
        true
    }
}

fn macro_invocation_text(lines: &[&str], start: usize) -> (String, usize) {
    let mut text = String::new();
    let mut depth = 0i32;
    let end = (start + 8).min(lines.len());

    for (index, line) in lines.iter().enumerate().take(end).skip(start) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(line.trim());

        for ch in line.chars() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
        }

        if depth <= 0 && text.contains('(') {
            return (text, index);
        }
    }

    (text, start)
}

fn declaration_block_has_generated_body(lines: &[&str], declaration_index: usize) -> bool {
    let end = (declaration_index + 20).min(lines.len());
    lines[declaration_index..end]
        .iter()
        .any(|line| line.contains("GENERATED_BODY") || line.contains("GENERATED_UCLASS_BODY"))
}

fn next_meaningful_line<'a>(lines: &'a [&str], start: usize) -> Option<(usize, &'a str)> {
    lines
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, line)| {
            let text = line.trim();
            !text.is_empty() && !text.starts_with("//")
        })
        .map(|(index, line)| (index, *line))
}

fn nearest_access_section(lines: &[&str], line_index: usize) -> Option<&'static str> {
    for line in lines[..line_index.min(lines.len())].iter().rev().take(80) {
        match line.trim() {
            "public:" => return Some("public"),
            "protected:" => return Some("protected"),
            "private:" => return Some("private"),
            _ => {}
        }
    }

    Some("private")
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_project_path(name: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ucore_diag_{name}_{stamp}"))
    }

    #[test]
    fn detects_missing_generated_body() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();
        let value = process_diagnostics(
            &conn,
            None,
            "UCLASS()\nclass AThing : public UObject {\n};\n",
            Some("C:/Project/AThing.h".to_string()),
        )
        .unwrap();
        let items = value["items"].as_array().unwrap();
        assert!(items.iter().any(|item| item["code"] == "UHT002"));
    }

    #[test]
    fn parses_msvc_build_errors() {
        let value = parse_build_diagnostics(
            r#"C:\Project\Source\Game\Thing.cpp(12,34): error C2065: 'Foo': undeclared identifier"#,
        );
        let items = value["items"].as_array().unwrap();
        assert_eq!(items[0]["line"], 11);
        assert_eq!(items[0]["character"], 33);
        assert_eq!(items[0]["severity"], "error");
    }

    #[test]
    fn does_not_require_generated_body_for_uenum() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();
        let value = process_diagnostics(
            &conn,
            None,
            "UENUM(BlueprintType)\nenum class EThing { One };\n",
            Some("C:/Project/EThing.h".to_string()),
        )
        .unwrap();
        let items = value["items"].as_array().unwrap();
        assert!(!items.iter().any(|item| item["code"] == "UHT002"));
    }

    #[test]
    fn detects_missing_cpp_definition_for_header_member_function() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        let root = temp_project_path("missing_impl");
        let header = root.join("Source/Game/Public/MyActor.h");
        std::fs::create_dir_all(header.parent().unwrap()).unwrap();

        let value = process_diagnostics(
            &conn,
            None,
            "class UMyActor\n{\npublic:\n    void DoThing();\n};\n",
            Some(header.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();

        let items = value["items"].as_array().unwrap();
        assert!(items.iter().any(|item| item["code"] == "UECPP001"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn incomplete_member_declaration_does_not_warn_about_missing_cpp() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        let root = temp_project_path("incomplete_decl");
        let header = root.join("Source/Game/Public/MyActor.h");
        std::fs::create_dir_all(header.parent().unwrap()).unwrap();

        let value = process_diagnostics(
            &conn,
            None,
            "class UMyActor\n{\npublic:\n    void DoThing()\n};\n",
            Some(header.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();

        let items = value["items"].as_array().unwrap();
        assert!(!items.iter().any(|item| item["code"] == "UECPP001"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn does_not_warn_when_matching_cpp_definition_exists() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        let root = temp_project_path("has_impl");
        let header = root.join("Source/Game/Public/MyActor.h");
        let source = root.join("Source/Game/Private/MyActor.cpp");
        std::fs::create_dir_all(header.parent().unwrap()).unwrap();
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(
            &source,
            "#include \"MyActor.h\"\n\nvoid UMyActor::DoThing()\n{\n}\n",
        )
        .unwrap();

        let value = process_diagnostics(
            &conn,
            None,
            "class UMyActor\n{\npublic:\n    void DoThing();\n};\n",
            Some(header.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();

        let items = value["items"].as_array().unwrap();
        assert!(!items.iter().any(|item| item["code"] == "UECPP001"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn constructor_with_default_argument_matches_cpp_definition() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        let root = temp_project_path("ctor_default_arg");
        let header = root.join("Source/Game/Public/MyActor.h");
        let source = root.join("Source/Game/Private/MyActor.cpp");
        std::fs::create_dir_all(header.parent().unwrap()).unwrap();
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(
            &source,
            "#include \"MyActor.h\"\n\nUMyActor::UMyActor(const FObjectInitializer& ObjectInitializer)\n{\n}\n",
        )
        .unwrap();

        let value = process_diagnostics(
            &conn,
            None,
            "class UMyActor\n{\npublic:\n    explicit UMyActor(const FObjectInitializer& ObjectInitializer = FObjectInitializer::Get());\n};\n",
            Some(header.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();

        let items = value["items"].as_array().unwrap();
        assert!(!items.iter().any(|item| item["code"] == "UECPP001"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unreal_style_header_missing_method_warns_without_false_ctor_warning() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        let root = temp_project_path("unreal_header_missing_impl");
        let header = root.join("Source/Game/Public/MyAbility.h");
        let source = root.join("Source/Game/Private/MyAbility.cpp");
        std::fs::create_dir_all(header.parent().unwrap()).unwrap();
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(
            &source,
            "#include \"MyAbility.h\"\n\nUMyAbility::UMyAbility(const FObjectInitializer& ObjectInitializer)\n{\n}\n",
        )
        .unwrap();

        let value = process_diagnostics(
            &conn,
            None,
            "UCLASS()\nclass UMyAbility\n{\n    GENERATED_BODY()\npublic:\n    explicit UMyAbility(const FObjectInitializer& ObjectInitializer = FObjectInitializer::Get());\nprivate:\n    void StartDeath();\n};\n",
            Some(header.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();

        let items = value["items"].as_array().unwrap();
        assert!(items.iter().any(|item| {
            item["code"] == "UECPP001"
                && item["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("StartDeath")
        }));
        assert!(!items.iter().any(|item| {
            item["code"] == "UECPP001"
                && item["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("UMyAbility::UMyAbility")
        }));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn blueprint_native_event_uses_implementation_suffix() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        let root = temp_project_path("blueprint_native_event_impl");
        let header = root.join("Source/Game/Public/MyAbility.h");
        let source = root.join("Source/Game/Private/MyAbility.cpp");
        std::fs::create_dir_all(header.parent().unwrap()).unwrap();
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(
            &source,
            "#include \"MyAbility.h\"\n\nvoid UMyAbility::OnDeath_Implementation()\n{\n}\n",
        )
        .unwrap();

        let value = process_diagnostics(
            &conn,
            None,
            "class UMyAbility\n{\npublic:\n    UFUNCTION(BlueprintNativeEvent)\n    void OnDeath();\n};\n",
            Some(header.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();

        let items = value["items"].as_array().unwrap();
        assert!(!items.iter().any(|item| item["code"] == "UECPP001"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rpc_with_validation_requires_validate_and_implementation() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        let root = temp_project_path("rpc_validate_impl");
        let header = root.join("Source/Game/Public/MyAbility.h");
        let source = root.join("Source/Game/Private/MyAbility.cpp");
        std::fs::create_dir_all(header.parent().unwrap()).unwrap();
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(
            &source,
            "#include \"MyAbility.h\"\n\nvoid UMyAbility::ServerFire_Implementation(int32 Count)\n{\n}\n",
        )
        .unwrap();

        let value = process_diagnostics(
            &conn,
            None,
            "class UMyAbility\n{\npublic:\n    UFUNCTION(Server, Reliable, WithValidation)\n    void ServerFire(int32 Count);\n};\n",
            Some(header.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();

        let items = value["items"].as_array().unwrap();
        assert!(items.iter().any(|item| {
            item["code"] == "UECPP001"
                && item["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("ServerFire_Validate")
        }));
        assert!(!items.iter().any(|item| {
            item["code"] == "UECPP001"
                && item["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("ServerFire_Implementation")
        }));

        let first = items
            .iter()
            .find(|item| item["code"] == "UECPP001")
            .expect("missing UECPP001");
        assert!(
            first["end_character"].as_u64().unwrap_or(0)
                > first["character"].as_u64().unwrap_or(0)
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn normalize_parameter_signature_strips_default_arguments() {
        let params = "(const FObjectInitializer& ObjectInitializer = FObjectInitializer::Get())";
        assert_eq!(
            normalize_parameter_signature(params),
            "(const FObjectInitializer& ObjectInitializer)"
        );
    }
}
