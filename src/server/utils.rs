use anyhow::Result;
use serde_json::Value;

/// Convert serde_json::Value into a typed request struct.
/// 把 serde_json::Value 转换成具体的请求结构体。
pub fn convert_params<T>(value: &Value) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    Ok(serde_json::from_value(value.clone())?)
}

/// Normalize a path to slash-separated form.
/// 把路径规范化成斜杠分隔形式。
pub fn normalize_to_unix(path: &str) -> String {
    strip_windows_unc_prefix(path).replace('\\', "/")
}

/// Normalize a path to the current platform's native separator.
/// 把路径规范化成当前平台的原生分隔符。
pub fn normalize_to_native(path: &str) -> String {
    let path = strip_windows_unc_prefix(path);

    if cfg!(target_os = "windows") {
        path.replace('/', "\\")
    } else {
        path.replace('\\', "/")
    }
}

/// Normalize a path for use as a stable project key.
/// 把路径规范化成稳定的工程 key。
pub fn normalize_path_key(path: &str) -> String {
    let mut normalized = normalize_to_unix(path);

    normalized = normalize_windows_drive_letter(&normalized);
    normalized = trim_trailing_slashes(&normalized);

    normalized
}

/// Return true if child path is inside parent path.
/// 判断 child 路径是否位于 parent 路径下。
pub fn path_starts_with(child: &str, parent: &str) -> bool {
    let child = normalize_path_key(child).to_ascii_lowercase();
    let parent = normalize_path_key(parent).to_ascii_lowercase();

    child == parent || child.starts_with(&(parent + "/"))
}

/// Remove Windows extended path prefix.
/// 移除 Windows 扩展路径前缀。
fn strip_windows_unc_prefix(path: &str) -> &str {
    path.strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix("//?/"))
        .unwrap_or(path)
}

/// Normalize Windows drive letter to uppercase.
/// 把 Windows 盘符统一成大写。
fn normalize_windows_drive_letter(path: &str) -> String {
    if cfg!(target_os = "windows") && path.len() >= 2 && path.as_bytes()[1] == b':' {
        let mut normalized = path.to_string();
        normalized.replace_range(0..1, &normalized[0..1].to_ascii_uppercase());
        normalized
    } else {
        path.to_string()
    }
}

/// Remove trailing slashes except root paths.
/// 删除末尾多余斜杠，但保留根路径。
fn trim_trailing_slashes(path: &str) -> String {
    let mut normalized = path.to_string();

    while normalized.ends_with('/') && !is_path_root(&normalized) {
        normalized.pop();
    }

    normalized
}

/// Return true if path looks like a root path.
/// 判断路径是否看起来是根路径。
fn is_path_root(path: &str) -> bool {
    path == "/" || path.len() == 3 && path.as_bytes()[1] == b':' && path.ends_with('/')
}
