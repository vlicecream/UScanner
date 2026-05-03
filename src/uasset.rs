use anyhow::{anyhow, Result};
use byteorder::{LittleEndian, ReadBytesExt};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const PACKAGE_TAG: u32 = 0x9E2A83C1;
const PACKAGE_TAG_SWAPPED: u32 = 0xC1832A9E;
const MAX_ANSI_STRING_LEN: i32 = 32 * 1024;
const MAX_WIDE_STRING_LEN: i32 = 16 * 1024;
const MAX_RECURSION_DEPTH: usize = 32;

/// One UObject import entry.
/// 一个 UObject Import 表条目。
#[derive(Debug, Clone)]
pub struct UObjectImport {
    pub object_name: String,
    pub class_name: String,
    pub outer_index: i32,
}

/// One UObject export entry.
/// 一个 UObject Export 表条目。
#[derive(Debug, Clone)]
pub struct UObjectExport {
    pub class_index: i32,
    pub super_index: i32,
    pub template_index: i32,
    pub outer_index: i32,
    pub object_name: String,
    pub serial_size: i64,
    pub serial_offset: i64,
}

/// Lightweight Unreal asset parser.
/// 轻量 Unreal 资产解析器。
#[derive(Debug, Default)]
pub struct UAssetParser {
    pub name_map: Vec<String>,
    pub import_map: Vec<UObjectImport>,
    pub export_map: Vec<UObjectExport>,
    pub imports: Vec<String>,
    pub functions: Vec<String>,
    pub parent_class: Option<String>,
    pub asset_name: String,
}

impl UAssetParser {
    /// Create an empty parser.
    /// 创建一个空 parser。
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a .uasset or .umap file.
    /// 解析一个 .uasset 或 .umap 文件。
    pub fn parse<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        self.clear();

        let path = path.as_ref();
        let file = File::open(path)?;
        let file_size = file.metadata()?.len();
        let mut reader = BufReader::new(file);

        let summary = PackageSummary::read(&mut reader, file_size)?;
        self.asset_name = summary.asset_name.clone();

        self.read_name_map(&mut reader, &summary)?;
        self.read_import_map(&mut reader, &summary)?;
        self.resolve_imports();

        self.read_export_map(&mut reader, &summary)?;
        self.resolve_parent_class();

        Ok(())
    }

    /// Reset parser state before parsing another file.
    /// 解析另一个文件前清空状态。
    fn clear(&mut self) {
        self.name_map.clear();
        self.import_map.clear();
        self.export_map.clear();
        self.imports.clear();
        self.functions.clear();
        self.parent_class = None;
        self.asset_name.clear();
    }

    /// Read NameMap from package.
    /// 从 package 读取 NameMap。
    fn read_name_map<R>(&mut self, reader: &mut R, summary: &PackageSummary) -> Result<()>
    where
        R: Read + Seek,
    {
        if summary.name_count < 0 {
            return Err(anyhow!("invalid name count: {}", summary.name_count));
        }

        seek_checked(reader, summary.name_offset, summary.file_size)?;

        for _ in 0..summary.name_count {
            let name = read_unreal_string(reader)?;
            self.name_map.push(name);

            if summary.ue4_version >= 504 {
                let _name_hash = reader.read_u32::<LittleEndian>()?;
            }
        }

        Ok(())
    }

    /// Read ImportMap from package.
    /// 从 package 读取 ImportMap。
    fn read_import_map<R>(&mut self, reader: &mut R, summary: &PackageSummary) -> Result<()>
    where
        R: Read + Seek,
    {
        if summary.import_count <= 0 || summary.import_offset <= 0 {
            return Ok(());
        }

        seek_checked(reader, summary.import_offset, summary.file_size)?;

        for _ in 0..summary.import_count {
            let _class_package = reader.read_i64::<LittleEndian>()?;
            let class_name_index = reader.read_i64::<LittleEndian>()? as i32;
            let outer_index = reader.read_i32::<LittleEndian>()?;
            let object_name_index = reader.read_i64::<LittleEndian>()? as i32;

            if !summary.has_unversioned_properties {
                let _optional_package_name = reader.read_i64::<LittleEndian>()?;
            }

            if summary.ue5_version >= 12 {
                let _import_optional_flags = reader.read_u32::<LittleEndian>()?;
            }

            self.import_map.push(UObjectImport {
                object_name: self.name_by_index(object_name_index),
                class_name: self.name_by_index(class_name_index),
                outer_index,
            });
        }

        Ok(())
    }

    /// Read ExportMap from package.
    /// 从 package 读取 ExportMap。
    fn read_export_map<R>(&mut self, reader: &mut R, summary: &PackageSummary) -> Result<()>
    where
        R: Read + Seek,
    {
        if summary.export_count <= 0 || summary.export_offset <= 0 {
            return Ok(());
        }

        seek_checked(reader, summary.export_offset, summary.file_size)?;

        for _ in 0..summary.export_count {
            let class_index = reader.read_i32::<LittleEndian>()?;
            let super_index = reader.read_i32::<LittleEndian>()?;

            let template_index = if summary.ue4_version >= 517 {
                reader.read_i32::<LittleEndian>()?
            } else {
                0
            };

            let outer_index = reader.read_i32::<LittleEndian>()?;
            let object_name_index = reader.read_i64::<LittleEndian>()?;
            let _object_flags = reader.read_u32::<LittleEndian>()?;

            let (serial_size, serial_offset) = if summary.ue4_version >= 511 {
                (
                    reader.read_i64::<LittleEndian>()?,
                    reader.read_i64::<LittleEndian>()?,
                )
            } else {
                (
                    reader.read_i32::<LittleEndian>()? as i64,
                    reader.read_i32::<LittleEndian>()? as i64,
                )
            };

            skip_export_tail(reader, summary)?;

            self.export_map.push(UObjectExport {
                class_index,
                super_index,
                template_index,
                outer_index,
                object_name: self.name_by_index(object_name_index as i32),
                serial_size,
                serial_offset,
            });
        }

        Ok(())
    }

    /// Resolve import paths into imports/functions.
    /// 把 ImportMap 解析成资源引用和函数引用。
    fn resolve_imports(&mut self) {
        for index in 0..self.import_map.len() {
            let object_index = -((index as i32) + 1);
            let path = self.resolve_object_path(object_index);

            if !path.starts_with('/') {
                continue;
            }

            if self.import_map[index].class_name == "Function" {
                self.functions.push(path);
            } else {
                self.imports.push(path);
            }
        }

        self.imports.sort();
        self.imports.dedup();

        self.functions.sort();
        self.functions.dedup();
    }

    /// Resolve Blueprint parent class from ExportMap.
    /// 从 ExportMap 推断蓝图父类。
    fn resolve_parent_class(&mut self) {
        let generated_class = format!("{}_C", self.asset_name);

        for export in &self.export_map {
            let is_self_export =
                export.object_name == self.asset_name || export.object_name == generated_class;

            if is_self_export && export.super_index != 0 {
                let parent = self.resolve_object_path(export.super_index);
                if parent != "None" {
                    self.parent_class = Some(parent);
                    return;
                }
            }
        }

        for export in &self.export_map {
            if export.outer_index != 0 || export.class_index >= 0 {
                continue;
            }

            let class_path = self.resolve_object_path(export.class_index);

            if class_path.starts_with("/Script/") && !class_path.contains("BlueprintGeneratedClass") {
                self.parent_class = Some(class_path);
                return;
            }
        }
    }

    /// Resolve UObject index into path-like string.
    /// 把 UObject index 解析成路径字符串。
    fn resolve_object_path(&self, index: i32) -> String {
        self.resolve_object_path_inner(index, 0)
    }

    /// Recursive UObject path resolver with depth guard.
    /// 带深度保护的递归 UObject 路径解析。
    fn resolve_object_path_inner(&self, index: i32, depth: usize) -> String {
        if index == 0 || depth > MAX_RECURSION_DEPTH {
            return "None".to_string();
        }

        if index < 0 {
            return self.resolve_import_path(index, depth);
        }

        self.resolve_export_path(index, depth)
    }

    /// Resolve import object path.
    /// 解析 import 对象路径。
    fn resolve_import_path(&self, index: i32, depth: usize) -> String {
        let import_index = (-index - 1) as usize;
        let Some(import) = self.import_map.get(import_index) else {
            return "None".to_string();
        };

        let object_name = import
            .object_name
            .strip_prefix("Default__")
            .unwrap_or(&import.object_name)
            .to_string();

        if import.outer_index == 0 {
            return object_name;
        }

        let outer = self.resolve_object_path_inner(import.outer_index, depth + 1);

        if outer == "None" {
            return object_name;
        }

        if outer.starts_with('/') {
            let separator = if import.class_name == "Function" { ":" } else { "." };
            format!("{}{}{}", outer, separator, object_name)
        } else {
            format!("{}/{}", outer, object_name)
        }
    }

    /// Resolve export object path.
    /// 解析 export 对象路径。
    fn resolve_export_path(&self, index: i32, depth: usize) -> String {
        let export_index = (index - 1) as usize;
        let Some(export) = self.export_map.get(export_index) else {
            return "None".to_string();
        };

        if export.outer_index == 0 {
            return export.object_name.clone();
        }

        let outer = self.resolve_object_path_inner(export.outer_index, depth + 1);

        if outer == "None" {
            return export.object_name.clone();
        }

        format!("{}.{}", outer, export.object_name)
    }

    /// Get name by NameMap index.
    /// 根据 NameMap index 获取名称。
    fn name_by_index(&self, index: i32) -> String {
        if index < 0 {
            return String::new();
        }

        self.name_map
            .get(index as usize)
            .cloned()
            .unwrap_or_default()
    }
}

// -----------------------------------------------------------------------------
// Package summary
// -----------------------------------------------------------------------------

/// Minimal package summary fields needed by this parser.
/// 这个 parser 需要的最小 PackageSummary 字段。
#[derive(Debug, Clone)]
struct PackageSummary {
    file_size: u64,
    _legacy_version: i32,
    ue4_version: i32,
    ue5_version: i32,
    has_unversioned_properties: bool,
    asset_name: String,
    name_count: i32,
    name_offset: i32,
    import_count: i32,
    import_offset: i32,
    export_count: i32,
    export_offset: i32,
}

impl PackageSummary {
    /// Read minimal package summary.
    /// 读取最小 package summary。
    fn read<R>(reader: &mut R, file_size: u64) -> Result<Self>
    where
        R: Read + Seek,
    {
        let tag = reader.read_u32::<LittleEndian>()?;

        if tag != PACKAGE_TAG && tag != PACKAGE_TAG_SWAPPED {
            return Err(anyhow!("invalid package tag: 0x{:X}", tag));
        }

        let legacy_version = reader.read_i32::<LittleEndian>()?;

        if legacy_version != -4 {
            let _legacy_ue3_version = reader.read_i32::<LittleEndian>()?;
        }

        let ue4_version = reader.read_i32::<LittleEndian>()?;

        let ue5_version = if legacy_version <= -8 {
            reader.read_i32::<LittleEndian>()?
        } else {
            0
        };

        let _licensee_version = reader.read_i32::<LittleEndian>()?;

        if legacy_version <= -9 {
            skip_bytes(reader, 20)?;
            let _total_header_size = reader.read_i32::<LittleEndian>()?;
        }

        if legacy_version <= -2 {
            let custom_version_count = reader.read_i32::<LittleEndian>()?;
            if (0..2000).contains(&custom_version_count) {
                skip_bytes(reader, custom_version_count as i64 * 20)?;
            }
        }

        if legacy_version > -9 {
            let _total_header_size = reader.read_i32::<LittleEndian>()?;
        }

        let package_name = read_unreal_string(reader)?;
        let asset_name = package_name.rsplit('/').next().unwrap_or("").to_string();

        let package_flags = reader.read_u32::<LittleEndian>()?;
        let has_unversioned_properties = (package_flags & 0x8000_0000) != 0;

        let name_count = reader.read_i32::<LittleEndian>()?;
        let name_offset = reader.read_i32::<LittleEndian>()?;

        if ue5_version >= 4 {
            skip_bytes(reader, 8)?;
        }

        if !has_unversioned_properties {
            let _localization_id = read_unreal_string(reader);
        }

        skip_bytes(reader, 8)?;

        let export_count = reader.read_i32::<LittleEndian>()?;
        let export_offset = reader.read_i32::<LittleEndian>()?;
        let import_count = reader.read_i32::<LittleEndian>()?;
        let import_offset = reader.read_i32::<LittleEndian>()?;

        let _depends_offset = reader.read_i32::<LittleEndian>()?;

        if ue4_version >= 515 {
            skip_bytes(reader, 8)?;
        }

        if ue4_version >= 516 {
            let _searchable_names_offset = reader.read_i32::<LittleEndian>()?;
        }

        let _thumbnail_table_offset = reader.read_i32::<LittleEndian>()?;

        if legacy_version > -9 {
            skip_bytes(reader, 16)?;
        }

        validate_table("name map", name_count, name_offset, file_size)?;
        validate_table("import map", import_count, import_offset, file_size)?;
        validate_table("export map", export_count, export_offset, file_size)?;

        Ok(Self {
            file_size,
            _legacy_version: legacy_version,
            ue4_version,
            ue5_version,
            has_unversioned_properties,
            asset_name,
            name_count,
            name_offset,
            import_count,
            import_offset,
            export_count,
            export_offset,
        })
    }
}

// -----------------------------------------------------------------------------
// Binary helpers
// -----------------------------------------------------------------------------

/// Read Unreal FString.
/// 读取 Unreal FString。
fn read_unreal_string<R>(reader: &mut R) -> Result<String>
where
    R: Read + Seek,
{
    let len = reader.read_i32::<LittleEndian>()?;

    if len == 0 {
        return Ok(String::new());
    }

    if len > 0 {
        if len > MAX_ANSI_STRING_LEN {
            return Err(anyhow!("ANSI string too long: {}", len));
        }

        let mut bytes = vec![0u8; len as usize];
        reader.read_exact(&mut bytes)?;

        if bytes.last() == Some(&0) {
            bytes.pop();
        }

        return Ok(String::from_utf8_lossy(&bytes).to_string());
    }

    let wide_len = -len;

    if wide_len > MAX_WIDE_STRING_LEN {
        return Err(anyhow!("wide string too long: {}", wide_len));
    }

    let mut units = Vec::with_capacity(wide_len as usize);

    for _ in 0..wide_len {
        units.push(reader.read_u16::<LittleEndian>()?);
    }

    if units.last() == Some(&0) {
        units.pop();
    }

    Ok(String::from_utf16_lossy(&units))
}

/// Seek to a checked offset.
/// 跳转到已检查的文件偏移。
fn seek_checked<R>(reader: &mut R, offset: i32, file_size: u64) -> Result<()>
where
    R: Seek,
{
    if offset <= 0 || offset as u64 >= file_size {
        return Err(anyhow!("invalid table offset: {}", offset));
    }

    reader.seek(SeekFrom::Start(offset as u64))?;
    Ok(())
}

/// Skip bytes safely.
/// 安全跳过指定字节数。
fn skip_bytes<R>(reader: &mut R, count: i64) -> Result<()>
where
    R: Seek,
{
    if count < 0 {
        return Err(anyhow!("cannot skip negative bytes: {}", count));
    }

    reader.seek(SeekFrom::Current(count))?;
    Ok(())
}

/// Validate table count and offset.
/// 校验表数量和偏移。
fn validate_table(name: &str, count: i32, offset: i32, file_size: u64) -> Result<()> {
    if count < 0 {
        return Err(anyhow!("{} count is negative: {}", name, count));
    }

    if count == 0 {
        return Ok(());
    }

    if offset <= 0 || offset as u64 >= file_size {
        return Err(anyhow!("{} offset is invalid: {}", name, offset));
    }

    Ok(())
}

/// Skip version-dependent export tail fields.
/// 跳过和版本相关的 Export 表尾部字段。
fn skip_export_tail<R>(reader: &mut R, summary: &PackageSummary) -> Result<()>
where
    R: Read + Seek,
{
    let _forced_export = reader.read_i32::<LittleEndian>()?;
    let _not_for_client = reader.read_i32::<LittleEndian>()?;
    let _not_for_server = reader.read_i32::<LittleEndian>()?;

    if summary.ue5_version < 1 {
        skip_bytes(reader, 16)?;
    }

    if summary.ue5_version >= 6 {
        let _is_inherited_instance = reader.read_i32::<LittleEndian>()?;
    }

    let _package_flags = reader.read_u32::<LittleEndian>()?;

    if summary.ue4_version >= 384 {
        let _not_always_loaded_for_editor_game = reader.read_i32::<LittleEndian>()?;
    }

    if summary.ue4_version >= 510 {
        let _is_asset = reader.read_i32::<LittleEndian>()?;
    }

    if summary.ue5_version >= 7 {
        let _generate_public_hash = reader.read_i32::<LittleEndian>()?;
    }

    if summary.ue4_version >= 504 {
        skip_bytes(reader, 20)?;
    }

    if summary.ue5_version >= 1 {
        skip_bytes(reader, 16)?;
    }

    Ok(())
}
