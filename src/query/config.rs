use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::server::state::AppState;
use crate::types::{
    ConfigCache, ConfigHistory, ConfigParameter, ConfigPlatform, ConfigSection,
};

const DEFAULT_SECTION: &str = "Default";
const MAX_INLINE_VALUE_LEN: usize = 50;

/// One parsed Unreal ini assignment.
/// 一个解析出来的 Unreal ini 配置项。
#[derive(Debug, Clone)]
struct IniItem {
    key: String,
    value: String,
    op: ConfigOp,
    line: usize,
}

/// Parsed ini file grouped by section.
/// 按 section 分组后的 ini 解析结果。
#[derive(Debug, Default)]
struct IniParsed {
    sections: HashMap<String, Vec<IniItem>>,
}

/// Unreal config operation prefix.
/// Unreal 配置操作符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigOp {
    Set,
    Add,
    Remove,
    Clear,
}

impl ConfigOp {
    /// Parse the operation prefix and return `(op, stripped_key)`.
    /// 解析操作符前缀，并返回 `(操作符, 去掉前缀后的 key)`。
    fn parse(key: &str) -> (Self, &str) {
        if let Some(stripped) = key.strip_prefix('+') {
            (Self::Add, stripped)
        } else if let Some(stripped) = key.strip_prefix('-') {
            (Self::Remove, stripped)
        } else if let Some(stripped) = key.strip_prefix('!') {
            (Self::Clear, stripped)
        } else {
            (Self::Set, key)
        }
    }

    /// Return the string form used by the UI.
    /// 返回 UI 使用的字符串形式。
    fn as_str(self) -> &'static str {
        match self {
            Self::Set => "",
            Self::Add => "+",
            Self::Remove => "-",
            Self::Clear => "!",
        }
    }
}

/// A config file in the merge stack.
/// 配置合并栈中的一个配置文件。
#[derive(Debug, Clone)]
struct ConfigSource {
    path: PathBuf,
    name: String,
}

/// Return config data with project-level cache.
/// 返回带项目级缓存的配置数据。
pub fn get_config_data_with_cache(
    state: &AppState,
    project_root_str: &str,
    engine_root_opt: Option<&str>,
) -> Result<Vec<ConfigPlatform>> {
    let root_key = crate::server::utils::normalize_path_key(project_root_str);

    {
        let caches = state.config_caches.lock();

        if let Some(cache) = caches.get(&root_key) {
            if !cache.is_dirty {
                return Ok(cache.data.clone());
            }
        }
    }

    let data = get_config_data_internal(project_root_str, engine_root_opt)
        .with_context(|| format!("failed to resolve config data for {}", project_root_str))?;

    {
        let mut caches = state.config_caches.lock();
        caches.insert(
            root_key,
            ConfigCache {
                data: data.clone(),
                is_dirty: false,
            },
        );
    }

    Ok(data)
}

/// Resolve config data for all available platforms.
/// 解析所有可用平台的配置数据。
fn get_config_data_internal(
    project_root_str: &str,
    engine_root_opt: Option<&str>,
) -> Result<Vec<ConfigPlatform>> {
    let project_root = PathBuf::from(project_root_str);
    let engine_root = engine_root_opt.map(PathBuf::from);

    let mut results = Vec::new();

    results.push(resolve_platform(
        "Default (Editor)",
        "Default",
        false,
        &project_root,
        engine_root.as_deref(),
    )?);

    for platform in get_available_platforms(&project_root, engine_root.as_deref()) {
        results.push(resolve_platform(
            &platform,
            &platform,
            false,
            &project_root,
            engine_root.as_deref(),
        )?);
    }

    Ok(results)
}

/// Parse a single `.ini` file.
/// 解析单个 `.ini` 文件。
fn parse_ini(path: &Path) -> Result<IniParsed> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read ini file {}", path.display()))?;

    let mut parsed = IniParsed::default();
    let mut current_section = DEFAULT_SECTION.to_string();

    for (index, raw_line) in content.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_inline_comment(raw_line).trim();

        if line.is_empty() {
            continue;
        }

        if let Some(section) = parse_section_header(line) {
            current_section = section.to_string();
            continue;
        }

        let Some((raw_key, raw_value)) = split_key_value(line) else {
            continue;
        };

        let (op, key) = ConfigOp::parse(raw_key.trim());

        if key.is_empty() {
            continue;
        }

        parsed
            .sections
            .entry(current_section.clone())
            .or_default()
            .push(IniItem {
                key: key.to_string(),
                value: raw_value.trim().to_string(),
                op,
                line: line_number,
            });
    }

    Ok(parsed)
}

/// Resolve one platform config by applying config stack order.
/// 按配置栈顺序合并某个平台的配置。
fn resolve_platform(
    name: &str,
    platform: &str,
    is_profile: bool,
    project_root: &Path,
    engine_root: Option<&Path>,
) -> Result<ConfigPlatform> {
    let stack = get_config_stack(project_root, engine_root, platform);

    let mut resolved: BTreeMap<String, BTreeMap<String, (Value, Vec<ConfigHistory>)>> =
        BTreeMap::new();

    for source in stack {
        let Ok(parsed) = parse_ini(&source.path) else {
            continue;
        };

        let file_name = source.name;
        let full_path = normalize_path(&source.path);

        for (section_name, items) in parsed.sections {
            let section = resolved.entry(section_name).or_default();

            for item in items {
                let entry = section
                    .entry(item.key.clone())
                    .or_insert_with(|| (Value::Null, Vec::new()));

                entry.0 = apply_op(
                    if entry.0.is_null() {
                        None
                    } else {
                        Some(entry.0.clone())
                    },
                    item.op,
                    &item.value,
                );

                entry.1.push(ConfigHistory {
                    file: file_name.clone(),
                    full_path: full_path.clone(),
                    value: format_value(&entry.0),
                    op: item.op.as_str().to_string(),
                    line: item.line,
                });
            }
        }
    }

    Ok(ConfigPlatform {
        name: name.to_string(),
        platform: platform.to_string(),
        is_profile,
        sections: resolved
            .into_iter()
            .map(|(section_name, params)| ConfigSection {
                name: section_name,
                parameters: params
                    .into_iter()
                    .map(|(key, (value, history))| ConfigParameter {
                        key,
                        value: format_value(&value),
                        history,
                    })
                    .collect(),
            })
            .collect(),
    })
}

/// Apply one Unreal config operation.
/// 应用一个 Unreal 配置操作。
fn apply_op(current: Option<Value>, op: ConfigOp, new_value: &str) -> Value {
    let new_value_json = Value::String(new_value.to_string());

    match op {
        ConfigOp::Clear => Value::Null,

        ConfigOp::Set => new_value_json,

        ConfigOp::Remove => match current {
            Some(Value::Array(mut values)) => {
                values.retain(|value| value != &new_value_json);
                if values.is_empty() {
                    Value::Null
                } else {
                    Value::Array(values)
                }
            }
            Some(value) if value == new_value_json => Value::Null,
            Some(value) => value,
            None => Value::Null,
        },

        ConfigOp::Add => match current {
            Some(Value::Array(mut values)) => {
                values.push(new_value_json);
                Value::Array(values)
            }
            Some(value) if value.is_null() => Value::Array(vec![new_value_json]),
            Some(value) => Value::Array(vec![value, new_value_json]),
            None => Value::Array(vec![new_value_json]),
        },
    }
}

/// Return user-friendly value text.
/// 返回适合 UI 显示的值。
fn format_value(value: &Value) -> String {
    match value {
        Value::Array(values) => {
            if let Some(last) = values.last() {
                format!(
                    "[Array x{}] {}",
                    values.len(),
                    last.as_str().unwrap_or("")
                )
            } else {
                "[]".to_string()
            }
        }
        Value::String(text) => truncate_for_display(text),
        Value::Null => "nil".to_string(),
        other => other.to_string(),
    }
}

/// Find available Unreal target platforms.
/// 查找可用的 Unreal 目标平台。
fn get_available_platforms(project_root: &Path, engine_root: Option<&Path>) -> Vec<String> {
    let mut platforms = BTreeSet::new();

    let mut dirs = Vec::new();

    if let Some(engine_root) = engine_root {
        dirs.push(engine_root.join("Engine/Config"));
        dirs.push(engine_root.join("Engine/Platforms"));
    }

    dirs.push(project_root.join("Config"));
    dirs.push(project_root.join("Platforms"));

    for dir in dirs {
        collect_platform_dirs(&dir, &mut platforms);
    }

    for platform in ["Windows", "Mac", "Linux", "Android", "IOS", "TVOS", "Apple", "Unix"] {
        if platform_exists(project_root, engine_root, platform) {
            platforms.insert(platform.to_string());
        }
    }

    platforms.into_iter().collect()
}

/// Build config merge stack for one platform.
/// 构建某个平台的配置合并栈。
fn get_config_stack(
    project_root: &Path,
    engine_root: Option<&Path>,
    platform: &str,
) -> Vec<ConfigSource> {
    let mut stack = Vec::new();

    if let Some(engine_root) = engine_root {
        push_if_exists(&mut stack, engine_root.join("Engine/Config/Base.ini"));
        push_if_exists(&mut stack, engine_root.join("Engine/Config/BaseEngine.ini"));

        if matches!(platform, "Mac" | "IOS" | "TVOS" | "Apple") {
            push_if_exists(&mut stack, engine_root.join("Engine/Config/Apple/AppleEngine.ini"));
        }

        if matches!(platform, "Linux" | "Unix") {
            push_if_exists(&mut stack, engine_root.join("Engine/Config/Unix/UnixEngine.ini"));
        }

        if platform != "Default" {
            push_if_exists(
                &mut stack,
                engine_root
                    .join("Engine/Config")
                    .join(platform)
                    .join(format!("{}Engine.ini", platform)),
            );
            push_all_ini_in_dir(
                &mut stack,
                engine_root
                    .join("Engine/Platforms")
                    .join(platform)
                    .join("Config"),
            );
        }
    }

    push_if_exists(&mut stack, project_root.join("Config/DefaultEngine.ini"));
    push_all_ini_in_dir(&mut stack, project_root.join("Platforms/Config"));

    if platform != "Default" {
        push_if_exists(
            &mut stack,
            project_root
                .join("Config")
                .join(platform)
                .join(format!("{}Engine.ini", platform)),
        );
        push_all_ini_in_dir(
            &mut stack,
            project_root
                .join("Platforms")
                .join(platform)
                .join("Config"),
        );
    }

    stack
}

/// Add config source if the file exists.
/// 文件存在时加入配置源。
fn push_if_exists(stack: &mut Vec<ConfigSource>, path: PathBuf) {
    if !path.is_file() {
        return;
    }

    let name = display_config_source_name(&path);
    stack.push(ConfigSource { path, name });
}

/// Add all `.ini` files in a directory in stable order.
/// 按稳定顺序加入目录下所有 `.ini` 文件。
fn push_all_ini_in_dir(stack: &mut Vec<ConfigSource>, dir: PathBuf) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let mut files = entries
        .flatten()
        .filter(|entry| entry.path().extension().map(|ext| ext == "ini").unwrap_or(false))
        .collect::<Vec<_>>();

    files.sort_by_key(|entry| entry.file_name());

    for entry in files {
        push_if_exists(stack, entry.path());
    }
}

/// Collect platform-like directories.
/// 收集可能的平台目录。
fn collect_platform_dirs(dir: &Path, platforms: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if !file_type.is_dir() && !file_type.is_symlink() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();

        if name.starts_with('.') {
            continue;
        }

        let path = entry.path();
        let checks = [
            path.join(format!("{}Engine.ini", name)),
            path.join("DataDrivenPlatformInfo.ini"),
            path.join("Config"),
        ];

        if checks.iter().any(|path| path.exists()) {
            platforms.insert(name);
        }
    }
}

/// Return true if a platform directory exists.
/// 判断平台目录是否存在。
fn platform_exists(project_root: &Path, engine_root: Option<&Path>, platform: &str) -> bool {
    let mut paths = Vec::new();

    if let Some(engine_root) = engine_root {
        paths.push(engine_root.join("Engine/Config").join(platform));
        paths.push(engine_root.join("Engine/Platforms").join(platform));
    }

    paths.push(project_root.join("Config").join(platform));
    paths.push(project_root.join("Platforms").join(platform));

    paths.iter().any(|path| path.is_dir())
}

/// Parse `[Section]`.
/// 解析 `[Section]`。
fn parse_section_header(line: &str) -> Option<&str> {
    line.strip_prefix('[')?.strip_suffix(']')
}

/// Split `key=value`.
/// 拆分 `key=value`。
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let pos = line.find('=')?;
    Some((&line[..pos], &line[pos + 1..]))
}

/// Strip inline comments only when they are at the start after trimming.
/// 只处理整行注释；不删除值里的 `;` 或 `#`。
fn strip_inline_comment(line: &str) -> &str {
    let trimmed = line.trim_start();

    if trimmed.starts_with(';') || trimmed.starts_with('#') {
        ""
    } else {
        line
    }
}

/// Build readable config source name.
/// 构建适合 UI 展示的配置源名称。
fn display_config_source_name(path: &Path) -> String {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    let Some(parent) = path.parent() else {
        return file_name;
    };

    if parent.file_name().map(|name| name == "Config").unwrap_or(false) {
        file_name
    } else {
        format!(
            "{}/{}",
            parent
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            file_name
        )
    }
}

/// Normalize path separators for UI output.
/// 规范化 UI 输出里的路径分隔符。
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Truncate long values for compact display.
/// 截断过长的值，便于紧凑展示。
fn truncate_for_display(text: &str) -> String {
    if text.chars().count() <= MAX_INLINE_VALUE_LEN {
        return text.to_string();
    }

    let truncated = text.chars().take(MAX_INLINE_VALUE_LEN - 3).collect::<String>();
    format!("{}...", truncated)
}
