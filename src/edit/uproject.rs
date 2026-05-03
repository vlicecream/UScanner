//! Utilities for editing `.uproject` / `.uplugin` JSON files.
//! 用于编辑 `.uproject` / `.uplugin` JSON 文件的工具。

use anyhow::{anyhow, Context, Result};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

/// Default indentation if none is detected in the source file.
/// 如果源文件中未检测到缩进，则默认使用 4 个空格。
const DEFAULT_INDENT: &str = "    ";

/// Adds a new module entry to the "Modules" array in a uproject/uplugin file.
/// 向 uproject/uplugin 文件的 "Modules" 数组中添加一个新的模块条目。
pub fn add_module(
    file_path: &str,
    module_name: &str,
    module_type: &str,
    loading_phase: &str,
) -> Result<()> {
    // 1. Basic validation of input strings.
    // 1. 对输入字符串进行基础校验。
    validate_field("module_name", module_name)?;
    validate_field("module_type", module_type)?;
    validate_field("loading_phase", loading_phase)?;

    // 2. Read file and detect formatting (newlines).
    // 2. 读取文件并检测格式（换行符类型）。
    let raw = std::fs::read_to_string(file_path)
        .with_context(|| format!("failed to read {}", file_path))?;
    let newline = detect_newline(&raw);
    let mut lines: Vec<String> = raw.lines().map(ToString::to_string).collect();

    // 3. Initialize Tree-sitter parser for JSON.
    // 3. 初始化用于 JSON 的 Tree-sitter 解析器。
    let language: tree_sitter::Language = tree_sitter_json::LANGUAGE.into();
    let mut parser = Parser::new();
    parser.set_language(&language)?;

    let source = raw.as_bytes();
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter-json failed to parse"))?;
    let root = tree.root_node();

    // Ensure the root of JSON is an object { ... }.
    // 确保 JSON 的根是一个对象 { ... }。
    let root_object = root
        .child(0)
        .filter(|node| node.kind() == "object")
        .ok_or_else(|| anyhow!("root JSON value must be an object"))?;

    // 4. Analyze the existing modules in the file.
    // 4. 分析文件中现有的模块信息。
    let info = collect_modules_info(root, source)?;

    // If the module already exists, do nothing and return.
    // 如果模块已经存在，则不执行任何操作直接返回。
    if info.module_names.iter().any(|name| *name == module_name) {
        return Ok(());
    }

    let indent = infer_indent(&lines).unwrap_or(DEFAULT_INDENT).to_string();

    // 5. Modification logic: Append to existing array OR Create new "Modules" block.
    // 5. 修改逻辑：追加到现有数组 或 创建新的 "Modules" 块。
    if info.modules_node.is_some() {
        // CASE: "Modules": [...] exists.
        // 情况 A: 已经存在 "Modules": [...] 数组。
        let last_module = info
            .last_module_node
            .ok_or_else(|| anyhow!("Modules exists but has no module object"))?;

        append_module_object(
            &mut lines,
            last_module,
            &info,
            &indent,
            module_name,
            module_type,
            loading_phase,
        )?;
    } else {
        // CASE: "Modules" key is missing, find an insertion point at the root.
        // 情况 B: 缺少 "Modules" 键，在根对象中寻找插入点。
        let last_pair = info
            .last_root_pair
            .or_else(|| last_child_of_kind(root_object, "pair"))
            .ok_or_else(|| anyhow!("could not find root insertion point"))?;

        insert_modules_block(
            &mut lines,
            last_pair,
            &info,
            &indent,
            module_name,
            module_type,
            loading_phase,
        )?;
    }

    // 6. Create backup and write changes to disk.
    // 6. 创建备份并将更改写入磁盘。
    let backup = format!("{}.old", file_path);
    std::fs::copy(file_path, &backup)
        .with_context(|| format!("failed to create backup {}", backup))?;
    std::fs::write(file_path, join_lines(&lines, newline))
        .with_context(|| format!("failed to write {}", file_path))?;

    Ok(())
}

/// Metadata about the Modules section found in the JSON syntax tree.
/// 关于在 JSON 语法树中找到的 Modules 章节的元数据。
#[derive(Default)]
struct ModulesInfo<'a> {
    modules_node: Option<Node<'a>>,       // The actual array [ ... ] / 实际的数组节点
    last_module_node: Option<Node<'a>>,   // Last object in the array / 数组中最后一个对象
    last_root_pair: Option<Node<'a>>,     // Last "Key": Value pair in root / 根对象中最后一组键值对
    root_pair_indent: Option<String>,    // Indentation of root keys / 根键的缩进
    module_object_indent: Option<String>, // Indentation of module objects / 模块对象的缩进
    module_names: Vec<&'a str>,           // Names of existing modules / 已有模块的名称列表
}

/// Uses Tree-sitter Queries to locate specific positions and data in the JSON.
/// 使用 Tree-sitter 查询 (Queries) 定位 JSON 中的特定位置和数据。
fn collect_modules_info<'a>(root: Node<'a>, source: &'a [u8]) -> Result<ModulesInfo<'a>> {
    let language: tree_sitter::Language = tree_sitter_json::LANGUAGE.into();
    // Tree-sitter S-expressions to find root pairs and specific "Modules" content.
    // 使用 Tree-sitter S-表达式查找根键值对以及特定的 "Modules" 内容。
    let query = Query::new(
        &language,
        r#"
        ; Find all top-level pairs in the root object
        (document (object (pair key: (string (string_content) @root.key) value: (_) @root.value) @root.pair))

        ; Find the Modules array and its items
        (document (object (pair key: (string (string_content) @modules.key) value: (array (object) @modules.item))))

        ; Extract names of modules already in the array
        (document (object (pair key: (string (string_content) @modules.name.array.key) value: (array (object (pair key: (string (string_content) @module.name.key) value: (string (string_content) @module.name.value)))))))
        "#,
    )?;

    let names = query.capture_names();
    let cap = |name: &str| names.iter().position(|n| *n == name).map(|i| i as u32);

    // Map query capture indices to local variables.
    // 将查询捕获索引映射到本地变量。
    let root_key = cap("root.key");
    let root_value = cap("root.value");
    let root_pair = cap("root.pair");
    let modules_key = cap("modules.key");
    let modules_item = cap("modules.item");
    let modules_name_array_key = cap("modules.name.array.key");
    let module_name_key = cap("module.name.key");
    let module_name_value = cap("module.name.value");

    let mut info = ModulesInfo::default();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    while let Some(m) = matches.next() {
        // Logic to identify if current match is specifically the "Modules" section.
        // 判断当前匹配项是否特别属于 "Modules" 部分的逻辑。
        let is_root_modules = m.captures.iter().any(|c| {
            Some(c.index) == root_key && node_text(c.node, source) == "Modules"
        });
        let is_modules_array = m.captures.iter().any(|c| {
            Some(c.index) == modules_key && node_text(c.node, source) == "Modules"
        });
        let is_module_name_query = m.captures.iter().any(|c| {
            Some(c.index) == modules_name_array_key && node_text(c.node, source) == "Modules"
        }) && m.captures.iter().any(|c| {
            Some(c.index) == module_name_key && node_text(c.node, source) == "Name"
        });

        for c in m.captures {
            if Some(c.index) == root_pair {
                info.last_root_pair = Some(c.node);
                info.root_pair_indent = Some(indent_before_node(c.node, source).to_string());
            }
            if is_root_modules && Some(c.index) == root_value {
                info.modules_node = Some(c.node);
            }
            if is_modules_array && Some(c.index) == modules_item {
                info.last_module_node = Some(c.node);
                info.module_object_indent = Some(indent_before_node(c.node, source).to_string());
            }
            if is_module_name_query && Some(c.index) == module_name_value {
                info.module_names.push(node_text(c.node, source));
            }
        }
    }

    Ok(info)
}

/// Appends a module object to an existing "Modules" array.
/// 将模块对象追加到现有的 "Modules" 数组中。
fn append_module_object(
    lines: &mut Vec<String>,
    last_module: Node,
    info: &ModulesInfo,
    indent: &str,
    name: &str,
    module_type: &str,
    loading_phase: &str,
) -> Result<()> {
    let row = last_module.end_position().row;
    let col = last_module.end_position().column;
    
    // 1. Add a comma after the previous last item.
    // 1. 在前一个末尾条目后添加逗号。
    split_line_at(lines, row, col)?;
    lines[row] = format!("{},", &lines[row][..col]);

    let item_indent = info
        .module_object_indent
        .clone()
        .unwrap_or_else(|| format!("{}{}", indent, indent));
    let field_indent = format!("{}{}", item_indent, indent);

    // 2. Insert the new module lines.
    // 2. 插入新的模块行。
    insert_lines(lines, row + 1, module_lines(&item_indent, &field_indent, name, module_type, loading_phase));
    Ok(())
}

/// Inserts a whole new "Modules": [ ... ] block into the root object.
/// 在根对象中插入一个全新的 "Modules": [ ... ] 块。
fn insert_modules_block(
    lines: &mut Vec<String>,
    last_pair: Node,
    info: &ModulesInfo,
    indent: &str,
    name: &str,
    module_type: &str,
    loading_phase: &str,
) -> Result<()> {
    let row = last_pair.end_position().row;
    let col = last_pair.end_position().column;

    // Add comma to the previous root pair.
    // 给前一个根键值对添加逗号。
    split_line_at(lines, row, col)?;
    lines[row] = format!("{},", &lines[row][..col]);

    let root_indent = info.root_pair_indent.clone().unwrap_or_else(|| indent.to_string());
    let item_indent = format!("{}{}", root_indent, indent);
    let field_indent = format!("{}{}", item_indent, indent);

    let mut block = Vec::new();
    block.push(format!("{}\"Modules\": [", root_indent));
    block.extend(module_lines(&item_indent, &field_indent, name, module_type, loading_phase));
    block.push(format!("{}]", root_indent));

    insert_lines(lines, row + 1, block);
    Ok(())
}

/// Generates the strings for a JSON module object.
/// 生成 JSON 模块对象的字符串列表。
fn module_lines(
    item_indent: &str,
    field_indent: &str,
    name: &str,
    module_type: &str,
    loading_phase: &str,
) -> Vec<String> {
    vec![
        format!("{}{{", item_indent),
        format!("{}\"Name\": \"{}\",", field_indent, escape_json(name)),
        format!("{}\"Type\": \"{}\",", field_indent, escape_json(module_type)),
        format!("{}\"LoadingPhase\": \"{}\"", field_indent, escape_json(loading_phase)),
        format!("{}}}", item_indent),
    ]
}

/// Helper to split a line at a specific column, creating a new line in the vector.
/// 辅助函数：在指定列切分行，在向量中创建新行。
fn split_line_at(lines: &mut Vec<String>, row: usize, col: usize) -> Result<()> {
    let line = lines.get(row).ok_or_else(|| anyhow!("row out of range"))?.clone();
    if col >= line.len() {
        return Ok(());
    }
    if !line.is_char_boundary(col) {
        return Err(anyhow!("split column is not a UTF-8 boundary"));
    }
    lines[row] = line[..col].to_string();
    lines.insert(row + 1, line[col..].to_string());
    Ok(())
}

/// Inserts multiple lines into the lines vector at a specific index.
/// 在特定索引处向行向量插入多行文本。
fn insert_lines(lines: &mut Vec<String>, index: usize, new_lines: Vec<String>) {
    for (offset, line) in new_lines.into_iter().enumerate() {
        lines.insert(index + offset, line);
    }
}

/// Finds the last child node of a specific grammar kind.
/// 寻找特定语法类型（如 "pair"）的最后一个子节点。
fn last_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut result = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            result = Some(child);
        }
    }
    result
}

/// Extracts the slice of source text corresponding to a node.
/// 提取节点对应的源文本切片。
fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[node.start_byte()..node.end_byte()]).unwrap_or("")
}

/// Gets the whitespace characters on the line immediately preceding the node.
/// 获取节点所在行之前的空白字符（用于检测缩进）。
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

/// Attempts to guess the indentation style from the existing lines.
/// 尝试从现有行中推断缩进风格。
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

/// Detects if the file uses CRLF (\r\n) or LF (\n).
/// 检测文件使用的是 CRLF 还是 LF 换行符。
fn detect_newline(raw: &str) -> &'static str {
    if raw.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Joins lines back into a single string with the detected newline.
/// 使用检测到的换行符将行重新合并为单个字符串。
fn join_lines(lines: &[String], newline: &str) -> String {
    let mut output = lines.join(newline);
    output.push_str(newline);
    output
}

/// Basic JSON string escaping (handles quotes, backslashes, etc.)
/// 基础 JSON 字符串转义（处理引号、反斜杠等）。
fn escape_json(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Validates that a string field is not empty and contains no invalid bytes.
/// 验证字符串字段不为空且不包含非法字节。
fn validate_field(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(anyhow!("{} cannot be empty", name));
    }
    if value.contains('\0') {
        return Err(anyhow!("{} contains NUL byte", name));
    }
    Ok(())
}