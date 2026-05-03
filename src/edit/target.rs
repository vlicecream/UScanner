//! Utilities for editing Unreal `.Target.cs` files.
//! 用于编辑 Unreal `.Target.cs` 文件的工具。

use anyhow::{anyhow, Context, Result};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

const DEFAULT_INDENT: &str = "    ";
const REGISTER_METHOD: &str = "RegisterModulesCreatedByNeovim";

/// Ensure a module is registered in a `.Target.cs` file.
/// 确保模块已注册到 `.Target.cs` 文件中。
pub fn add_module(file_path: &str, module_name: &str) -> Result<()> {
    validate_module_name(module_name)?;

    let raw = std::fs::read_to_string(file_path)
        .with_context(|| format!("failed to read {}", file_path))?;
    let newline = detect_newline(&raw);
    let mut lines: Vec<String> = raw.lines().map(ToString::to_string).collect();

    let language: tree_sitter::Language = tree_sitter_c_sharp::LANGUAGE.into();
    let mut parser = Parser::new();
    parser.set_language(&language)?;

    let source = raw.as_bytes();
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter-c-sharp failed to parse"))?;
    let root = tree.root_node();

    let info = collect_target_info(root, source)?;
    let indent = info
        .constructor_indent
        .clone()
        .or_else(|| infer_indent(&lines).map(ToString::to_string))
        .unwrap_or_else(|| DEFAULT_INDENT.to_string());

    let mut edits = Vec::new();

    if !info.constructor_has_register_call {
        let insertion = info
            .constructor_last_statement
            .ok_or_else(|| anyhow!("TargetRules constructor has no statement insertion point"))?;

        let call_indent = format!("{}{}", indent, indent);
        edits.push(Edit::insert_after(
            insertion.end_position().row,
            vec![format!("{}{}();", call_indent, REGISTER_METHOD)],
        ));
    }

    if info.register_method_found {
        if info.add_range_found {
            if !info.module_found {
                let edit = build_append_module_edit(&info, &indent, module_name)?;
                edits.push(edit);
            }
        } else {
            let insertion = info
                .register_method_last_statement
                .or(info.register_method_body)
                .ok_or_else(|| anyhow!("Register method exists but has no insertion point"))?;

            let body_indent = format!("{}{}", indent, indent);
            let item_indent = format!("{}{}", body_indent, indent);
            edits.push(Edit::insert_after(
                insertion.end_position().row,
                vec![
                    format!("{}ExtraModuleNames.AddRange(new string[] {{", body_indent),
                    format!("{}\"{}\"", item_indent, escape_csharp_string(module_name)),
                    format!("{}}});", body_indent),
                ],
            ));
        }
    } else {
        let insertion = info
            .last_target_member
            .ok_or_else(|| anyhow!("could not find insertion point in TargetRules class"))?;

        let method_indent = indent.clone();
        let body_indent = format!("{}{}", indent, indent);
        let item_indent = format!("{}{}", body_indent, indent);

        edits.push(Edit::insert_after(
            insertion.end_position().row,
            vec![
                String::new(),
                format!("{}private void {}()", method_indent, REGISTER_METHOD),
                format!("{}{{", method_indent),
                format!("{}ExtraModuleNames.AddRange(new string[] {{", body_indent),
                format!("{}\"{}\"", item_indent, escape_csharp_string(module_name)),
                format!("{}}});", body_indent),
                format!("{}}}", method_indent),
            ],
        ));
    }

    apply_edits(&mut lines, edits)?;

    let backup = format!("{}.old", file_path);
    std::fs::copy(file_path, &backup)
        .with_context(|| format!("failed to create backup {}", backup))?;
    std::fs::write(file_path, join_lines(&lines, newline))
        .with_context(|| format!("failed to write {}", file_path))?;

    Ok(())
}

/// Information collected from the TargetRules class.
/// 从 TargetRules 类中收集到的信息。
#[derive(Default)]
struct TargetInfo<'a> {
    constructor_indent: Option<String>,
    constructor_has_register_call: bool,
    constructor_last_statement: Option<Node<'a>>,
    register_method_found: bool,
    register_method_body: Option<Node<'a>>,
    register_method_last_statement: Option<Node<'a>>,
    add_range_found: bool,
    module_found: bool,
    initializer_expr: Option<Node<'a>>,
    last_module_literal: Option<Node<'a>>,
    last_target_member: Option<Node<'a>>,
}

/// A line-based edit.
/// 基于行的编辑操作。
struct Edit {
    row: usize,
    lines: Vec<String>,
}

impl Edit {
    /// Insert lines after a row.
    /// 在指定行之后插入多行。
    fn insert_after(row: usize, lines: Vec<String>) -> Self {
        Self { row, lines }
    }
}

/// Collect all relevant anchors from the `.Target.cs` syntax tree.
/// 从 `.Target.cs` 语法树里收集所有需要的锚点。
fn collect_target_info<'a>(root: Node<'a>, source: &'a [u8]) -> Result<TargetInfo<'a>> {
    let language: tree_sitter::Language = tree_sitter_c_sharp::LANGUAGE.into();

    let query = Query::new(
        &language,
        r#"
        ; TargetRules constructor and its statements.
        ; TargetRules 构造函数及其语句。
        (compilation_unit
          (class_declaration
            (base_list (identifier) @class.base)
            body: (declaration_list
              (constructor_declaration
                body: (block (_) @constructor.statement)
              ) @constructor.decl
            )
          )
        )

        ; RegisterModulesCreatedByNeovim invocation inside constructor.
        ; 构造函数内的 RegisterModulesCreatedByNeovim 调用。
        (compilation_unit
          (class_declaration
            (base_list (identifier) @class.base)
            body: (declaration_list
              (constructor_declaration
                body: (block
                  (expression_statement
                    (invocation_expression
                      function: (identifier) @constructor.call
                    )
                  )
                )
              )
            )
          )
        )

        ; TargetRules class members.
        ; TargetRules 类成员。
        (compilation_unit
          (class_declaration
            (base_list (identifier) @class.base)
            body: (declaration_list
              (_) @target.member
            )
          )
        )

        ; Register method and its statements.
        ; 注册方法及其语句。
        (compilation_unit
          (class_declaration
            (base_list (identifier) @class.base)
            body: (declaration_list
              (method_declaration
                name: (identifier) @register.method.name
                body: (block (_) @register.method.statement)
              ) @register.method.decl
            )
          )
        )

        ; Register method body even when empty.
        ; 即使注册方法为空，也捕获它的方法体。
        (compilation_unit
          (class_declaration
            (base_list (identifier) @class.base)
            body: (declaration_list
              (method_declaration
                name: (identifier) @register.empty.method.name
                body: (block) @register.method.body
              )
            )
          )
        )

        ; Existing AddRange initializer.
        ; 已存在的 AddRange 初始化器。
        (compilation_unit
          (class_declaration
            (base_list (identifier) @class.base)
            body: (declaration_list
              (method_declaration
                name: (identifier) @register.addrange.method.name
                body: (block
                  (expression_statement
                    (invocation_expression
                      function: (member_access_expression) @addrange.call
                      arguments: (argument_list
                        (argument
                          (array_creation_expression
                            (initializer_expression
                              (string_literal)* @module.literal
                            ) @initializer.expr
                          )
                        )
                      )
                    )
                  )
                )
              )
            )
          )
        )

        ; Module string values inside AddRange.
        ; AddRange 里的模块字符串。
        (compilation_unit
          (class_declaration
            (base_list (identifier) @class.base)
            body: (declaration_list
              (method_declaration
                name: (identifier) @register.module.method.name
                body: (block
                  (expression_statement
                    (invocation_expression
                      function: (member_access_expression) @module.addrange.call
                      arguments: (argument_list
                        (argument
                          (array_creation_expression
                            (initializer_expression
                              (string_literal
                                (string_literal_content) @module.name
                              )
                            )
                          )
                        )
                      )
                    )
                  )
                )
              )
            )
          )
        )
        "#,
    )?;

    let names = query.capture_names();
    let cap = |name: &str| names.iter().position(|n| *n == name).map(|i| i as u32);

    let class_base = cap("class.base");
    let constructor_decl = cap("constructor.decl");
    let constructor_statement = cap("constructor.statement");
    let constructor_call = cap("constructor.call");
    let target_member = cap("target.member");
    let register_method_name = cap("register.method.name");
    let register_method_statement = cap("register.method.statement");
    let register_empty_method_name = cap("register.empty.method.name");
    let register_method_body = cap("register.method.body");
    let register_addrange_method_name = cap("register.addrange.method.name");
    let addrange_call = cap("addrange.call");
    let initializer_expr = cap("initializer.expr");
    let module_literal = cap("module.literal");
    let register_module_method_name = cap("register.module.method.name");
    let module_addrange_call = cap("module.addrange.call");
    let module_name = cap("module.name");

    let mut info = TargetInfo::default();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    while let Some(m) = matches.next() {
        let is_target_rules = m.captures.iter().any(|c| {
            Some(c.index) == class_base && node_text(c.node, source) == "TargetRules"
        });

        if !is_target_rules {
            continue;
        }

        let is_register_method = m.captures.iter().any(|c| {
            Some(c.index) == register_method_name
                && node_text(c.node, source) == REGISTER_METHOD
        });

        let is_register_empty_method = m.captures.iter().any(|c| {
            Some(c.index) == register_empty_method_name
                && node_text(c.node, source) == REGISTER_METHOD
        });

        let is_addrange_register_method = m.captures.iter().any(|c| {
            Some(c.index) == register_addrange_method_name
                && node_text(c.node, source) == REGISTER_METHOD
        });

        let is_module_register_method = m.captures.iter().any(|c| {
            Some(c.index) == register_module_method_name
                && node_text(c.node, source) == REGISTER_METHOD
        });

        for c in m.captures {
            if Some(c.index) == constructor_decl {
                info.constructor_indent = Some(indent_before_node(c.node, source).to_string());
            }

            if Some(c.index) == constructor_statement {
                info.constructor_last_statement = Some(c.node);
            }

            if Some(c.index) == constructor_call
                && node_text(c.node, source) == REGISTER_METHOD
            {
                info.constructor_has_register_call = true;
            }

            if Some(c.index) == target_member {
                info.last_target_member = Some(c.node);
            }

            if is_register_method && Some(c.index) == register_method_statement {
                info.register_method_found = true;
                info.register_method_last_statement = Some(c.node);
            }

            if is_register_empty_method && Some(c.index) == register_method_body {
                info.register_method_found = true;
                info.register_method_body = Some(c.node);
            }

            if is_addrange_register_method && Some(c.index) == addrange_call
                && node_text(c.node, source) == "ExtraModuleNames.AddRange"
            {
                info.add_range_found = true;
            }

            if is_addrange_register_method && Some(c.index) == initializer_expr {
                info.initializer_expr = Some(c.node);
            }

            if is_addrange_register_method && Some(c.index) == module_literal {
                info.last_module_literal = Some(c.node);
            }

            if is_module_register_method && Some(c.index) == module_addrange_call
                && node_text(c.node, source) == "ExtraModuleNames.AddRange"
            {
                info.add_range_found = true;
            }

            if is_module_register_method && Some(c.index) == module_name {
                // The caller compares this text later by editing state.
                // 调用方通过外层逻辑比较模块名。
            }
        }
    }

    Ok(info)
}

/// Build the edit that appends a missing module to an existing AddRange.
/// 构建向已有 AddRange 追加模块的编辑。
fn build_append_module_edit<'a>(
    info: &TargetInfo<'a>,
    indent: &str,
    module_name: &str,
) -> Result<Edit> {
    if let Some(last_module) = info.last_module_literal {
        let row = last_module.end_position().row;
        let module_indent = format!("{}{}{}", indent, indent, indent);
        Ok(Edit::insert_after(
            row,
            vec![format!("{}\"{}\"", module_indent, escape_csharp_string(module_name))],
        ))
    } else {
        let initializer = info
            .initializer_expr
            .ok_or_else(|| anyhow!("AddRange exists but initializer expression is missing"))?;

        let row = initializer.start_position().row;
        let module_indent = format!("{}{}{}", indent, indent, indent);
        Ok(Edit::insert_after(
            row,
            vec![format!("{}\"{}\"", module_indent, escape_csharp_string(module_name))],
        ))
    }
}

/// Apply edits from bottom to top to avoid row offset bugs.
/// 从下往上应用编辑，避免行号偏移 bug。
fn apply_edits(lines: &mut Vec<String>, mut edits: Vec<Edit>) -> Result<()> {
    edits.sort_by_key(|edit| edit.row);
    edits.reverse();

    for edit in edits {
        let index = edit.row + 1;
        if index > lines.len() {
            return Err(anyhow!("edit row {} is out of range", edit.row));
        }

        for (offset, line) in edit.lines.into_iter().enumerate() {
            lines.insert(index + offset, line);
        }

        if edit_needs_comma(lines, edit.row) {
            lines[edit.row].push(',');
        }
    }

    Ok(())
}

/// Decide whether an edit target line should receive a trailing comma.
/// 判断编辑目标行是否需要补逗号。
fn edit_needs_comma(lines: &[String], row: usize) -> bool {
    let Some(line) = lines.get(row) else {
        return false;
    };

    let trimmed = line.trim_end();
    trimmed.starts_with('"') && !trimmed.ends_with(',') && !trimmed.ends_with('{')
}

/// Return text of a node.
/// 返回节点文本。
fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[node.start_byte()..node.end_byte()]).unwrap_or("")
}

/// Return indentation before a node.
/// 返回节点前的缩进。
fn indent_before_node<'a>(node: Node, source: &'a [u8]) -> &'a str {
    let start = node.start_byte();
    let line_start = source[..start]
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let prefix = &source[line_start..start];
    let len = prefix
        .iter()
        .take_while(|b| **b == b' ' || **b == b'\t')
        .count();
    std::str::from_utf8(&prefix[..len]).unwrap_or("")
}

/// Infer indentation from existing lines.
/// 从已有行推断缩进。
fn infer_indent(lines: &[String]) -> Option<&str> {
    for line in lines {
        let len = line
            .as_bytes()
            .iter()
            .take_while(|b| **b == b' ' || **b == b'\t')
            .count();

        if len > 0 {
            return Some(&line[..len]);
        }
    }

    None
}

/// Detect file newline style.
/// 检测文件换行风格。
fn detect_newline(raw: &str) -> &'static str {
    if raw.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// Join lines while preserving newline style.
/// 按原换行风格拼接行。
fn join_lines(lines: &[String], newline: &str) -> String {
    let mut output = lines.join(newline);
    output.push_str(newline);
    output
}

/// Escape C# string content.
/// 转义 C# 字符串内容。
fn escape_csharp_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Validate module name before writing it into C#.
/// 写入 C# 前校验模块名。
fn validate_module_name(module_name: &str) -> Result<()> {
    if module_name.trim().is_empty() {
        return Err(anyhow!("module_name cannot be empty"));
    }

    if module_name.contains('\0') {
        return Err(anyhow!("module_name contains NUL byte"));
    }

    Ok(())
}
