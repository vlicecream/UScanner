use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::PathBuf;

// -----------------------------------------------------------------------------
// Top-level requests
// -----------------------------------------------------------------------------

/// Raw request used by non-RPC or legacy stdin/stdout entry points.
/// 非 RPC 或旧 stdin/stdout 入口使用的原始请求。
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum RawRequest {
    #[serde(rename = "refresh")]
    Refresh(RefreshRequest),

    #[serde(rename = "scan")]
    Scan(ScanRequest),
}

/// Request to scan a batch of files.
/// 扫描一批文件的请求。
#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    pub files: Vec<InputFile>,
}

/// Request to refresh the whole project index.
/// 刷新整个工程索引的请求。
#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    #[serde(rename = "type")]
    pub msg_type: String,

    pub project_root: String,
    pub engine_root: Option<String>,
    pub db_path: Option<String>,
    pub cache_db_path: Option<String>,

    #[serde(default)]
    pub config: UEPConfig,

    #[serde(default)]
    pub scope: Option<String>,

    #[serde(default)]
    pub vcs_hash: Option<String>,
}

/// Request to start watching a project.
/// 启动工程文件监听的请求。
#[derive(Debug, Deserialize, Serialize)]
pub struct WatchRequest {
    pub project_root: String,

    #[serde(default)]
    pub db_path: Option<String>,
}

/// Request to register/setup a project.
/// 注册或初始化工程的请求。
#[derive(Debug, Deserialize, Serialize)]
pub struct SetupRequest {
    pub project_root: String,
    pub db_path: String,

    #[serde(default)]
    pub cache_db_path: Option<String>,

    #[serde(default)]
    pub config: UEPConfig,

    #[serde(default)]
    pub vcs_hash: Option<String>,
}

/// User config for project indexing.
/// 工程索引配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UEPConfig {
    #[serde(default)]
    pub excludes_directory: Vec<String>,

    #[serde(default)]
    pub include_extensions: Vec<String>,
}

impl Default for UEPConfig {
    fn default() -> Self {
        Self {
            excludes_directory: vec![
                "Intermediate".to_string(),
                "Binaries".to_string(),
                "Build".to_string(),
                "Saved".to_string(),
                ".git".to_string(),
                ".vs".to_string(),
            ],
            include_extensions: vec![
                "h".to_string(),
                "hh".to_string(),
                "hpp".to_string(),
                "cpp".to_string(),
                "cc".to_string(),
                "cxx".to_string(),
                "inl".to_string(),
                "cs".to_string(),
                "ini".to_string(),
                "uasset".to_string(),
                "umap".to_string(),
                "uproject".to_string(),
                "uplugin".to_string(),
            ],
        }
    }
}

// -----------------------------------------------------------------------------
// Parser input/output
// -----------------------------------------------------------------------------

/// Input file passed to parser/scanner.
/// 传给 parser/scanner 的单个输入文件。
#[derive(Debug, Clone, Deserialize)]
pub struct InputFile {
    pub path: String,
    pub mtime: u64,

    #[serde(default)]
    pub old_hash: Option<String>,

    #[serde(default)]
    pub module_id: Option<i64>,

    #[serde(default)]
    pub db_path: Option<String>,
}

/// Single-file parse result.
/// 单文件解析结果。
#[derive(Debug, Clone, Serialize)]
pub struct ParseResult {
    pub path: String,
    pub status: String,
    pub mtime: u64,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<ParseData>,

    #[serde(skip)]
    pub module_id: Option<i64>,
}

/// Parsed data extracted from one source file.
/// 从单个源码文件解析出的数据。
#[derive(Debug, Clone, Serialize)]
pub struct ParseData {
    pub classes: Vec<ClassInfo>,
    pub calls: Vec<CallInfo>,
    pub includes: Vec<String>,
    pub parser: String,
    pub new_hash: String,
}

/// One function or symbol call.
/// 一次函数或符号调用。
#[derive(Debug, Clone, Serialize)]
pub struct CallInfo {
    pub name: String,
    pub line: usize,
}

/// Class, struct, enum, typedef, or Unreal reflected type.
/// class、struct、enum、typedef 或 Unreal 反射类型。
#[derive(Debug, Clone, Serialize)]
pub struct ClassInfo {
    pub class_name: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,

    pub base_classes: Vec<String>,
    pub symbol_type: String,
    pub line: usize,
    pub end_line: usize,

    #[serde(skip)]
    pub range_start: usize,

    #[serde(skip)]
    pub range_end: usize,

    pub members: Vec<MemberInfo>,
    pub is_final: bool,
    pub is_interface: bool,
}

/// Class member, function, property, or enum item.
/// 类成员、函数、属性或枚举项。
#[derive(Debug, Clone, Serialize)]
pub struct MemberInfo {
    pub name: String,

    #[serde(rename = "type")]
    pub mem_type: String,

    pub flags: String,
    pub access: String,
    pub line: usize,
    pub end_line: usize,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_type: Option<String>,
}

// -----------------------------------------------------------------------------
// Progress reporting
// -----------------------------------------------------------------------------

/// Progress event sent to client.
/// 发送给客户端的进度事件。
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    #[serde(rename = "type")]
    pub msg_type: String,

    pub stage: String,
    pub current: usize,
    pub total: usize,
    pub message: String,
}

/// One refresh phase used by progress UI.
/// progress UI 使用的单个 refresh 阶段。
#[derive(Debug, Clone, Serialize)]
pub struct PhaseInfo {
    pub name: String,
    pub label: String,
    pub weight: f64,
}

/// Refresh phase plan sent before progress starts.
/// refresh 开始前发送的阶段计划。
#[derive(Debug, Clone, Serialize)]
pub struct ProgressPlan {
    #[serde(rename = "type")]
    pub msg_type: String,

    pub phases: Vec<PhaseInfo>,
}

/// Progress reporter abstraction.
/// 进度上报抽象接口。
pub trait ProgressReporter: Send + Sync {
    /// Report current progress.
    /// 上报当前进度。
    fn report(&self, stage: &str, current: usize, total: usize, message: &str);

    /// Report refresh phase plan.
    /// 上报 refresh 阶段计划。
    fn report_plan(&self, phases: &[PhaseInfo]);
}

/// Stdout-based progress reporter for CLI fallback.
/// 基于 stdout 的进度上报器，用于 CLI 兜底。
pub struct StdoutReporter;

impl ProgressReporter for StdoutReporter {
    fn report(&self, stage: &str, current: usize, total: usize, message: &str) {
        let progress = Progress {
            msg_type: "progress".to_string(),
            stage: stage.to_string(),
            current,
            total,
            message: message.to_string(),
        };

        write_json_line(&progress);
    }

    fn report_plan(&self, phases: &[PhaseInfo]) {
        let plan = ProgressPlan {
            msg_type: "progress_plan".to_string(),
            phases: phases.to_vec(),
        };

        write_json_line(&plan);
    }
}

/// Convenience helper for old code paths.
/// 给旧代码路径使用的便捷进度函数。
pub fn report_progress(stage: &str, current: usize, total: usize, message: &str) {
    StdoutReporter.report(stage, current, total, message);
}

/// Write one JSON line to stdout.
/// 向 stdout 写入一行 JSON。
fn write_json_line<T: Serialize>(value: &T) {
    if let Ok(mut text) = serde_json::to_string(value) {
        text.push('\n');

        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    }
}

// -----------------------------------------------------------------------------
// Project/module/component data
// -----------------------------------------------------------------------------

/// Discovered Unreal module definition.
/// 扫描发现的 Unreal 模块定义。
#[derive(Debug, Clone)]
pub struct ModuleDef {
    pub name: String,
    pub path: PathBuf,
    pub root: PathBuf,
    pub public_deps: Vec<String>,
    pub private_deps: Vec<String>,
    pub mod_type: String,
    pub owner_name: String,
    pub component_name: Option<String>,
}

/// Discovered project component: Game, Engine, or Plugin.
/// 扫描发现的工程组件：Game、Engine 或 Plugin。
#[derive(Debug, Clone)]
pub struct ComponentDef {
    pub name: String,
    pub display_name: String,
    pub comp_type: String,
    pub root_path: PathBuf,
    pub uproject_path: Option<PathBuf>,
    pub uplugin_path: Option<PathBuf>,
    pub owner_name: String,
}

/// Minimal .uproject/.uplugin JSON shape.
/// .uproject/.uplugin 的最小 JSON 结构。
#[derive(Debug, Deserialize)]
pub struct UProjectPluginJson {
    #[serde(rename = "Modules")]
    pub modules: Option<Vec<UModuleJson>>,
}

/// Module entry inside .uproject/.uplugin.
/// .uproject/.uplugin 里的模块条目。
#[derive(Debug, Deserialize)]
pub struct UModuleJson {
    #[serde(rename = "Name")]
    pub name: String,

    #[serde(rename = "Type")]
    pub mod_type: String,
}

// -----------------------------------------------------------------------------
// Unreal config query data
// -----------------------------------------------------------------------------

/// Resolved config for one platform.
/// 某个平台解析后的配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPlatform {
    pub name: String,
    pub platform: String,
    pub is_profile: bool,
    pub sections: Vec<ConfigSection>,
}

/// One config section.
/// 一个配置 section。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSection {
    pub name: String,
    pub parameters: Vec<ConfigParameter>,
}

/// One config parameter and its history.
/// 一个配置参数以及它的来源历史。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigParameter {
    pub key: String,
    pub value: String,
    pub history: Vec<ConfigHistory>,
}

/// One config override history entry.
/// 一条配置覆盖历史。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigHistory {
    pub file: String,
    pub full_path: String,
    pub value: String,
    pub op: String,
    pub line: usize,
}

/// In-memory config query cache.
/// 内存配置查询缓存。
#[derive(Debug, Clone)]
pub struct ConfigCache {
    pub data: Vec<ConfigPlatform>,
    pub is_dirty: bool,
}

// -----------------------------------------------------------------------------
// Query protocol
// -----------------------------------------------------------------------------

/// Query requests sent from Neovim/Lua to Rust server.
/// Neovim/Lua 发给 Rust server 的查询请求。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind")]
pub enum QueryRequest {
    // Derived/inheritance queries.
    // 派生和继承查询。
    FindDerivedClasses { base_class: String },
    GetRecursiveDerivedClasses { base_class: String },
    GetRecursiveParentClasses { child_class: String },
    FindSymbolInInheritanceChain {
        class_name: String,
        symbol_name: String,
        #[serde(default)]
        mode: Option<String>,
    },
    GetVirtualFunctionsInInheritanceChain { class_name: String },

    // Class/type/member queries.
    // 类、类型、成员查询。
    FindClassByName { name: String },
    SearchClassesPrefix { prefix: String, limit: Option<usize> },
    GetClasses {
        #[serde(default)]
        extra_where: Option<String>,
        #[serde(default)]
        params: Option<Vec<String>>,
    },
    GetStructs {
        #[serde(default)]
        extra_where: Option<String>,
        #[serde(default)]
        params: Option<Vec<String>>,
    },
    GetStructsOnly,
    GetFileSymbols { file_path: String },
    GetClassFilePath { class_name: String },
    GetClassMembersById { class_id: i64 },
    GetClassMembers { class_name: String },
    GetClassMethods { class_name: String },
    GetClassProperties { class_name: String },
    GetClassMembersRecursive {
        class_name: String,
        #[serde(default)]
        namespace: Option<String>,
    },
    GetEnumValues { enum_name: String },

    // Module/component queries.
    // 模块和组件查询。
    GetComponents,
    GetModules,
    GetModuleByName { name: String },
    GetModuleIdByName { name: String },
    GetModuleRootPath { name: String },
    GetFilesInModule { module_id: i64 },
    GetFilesInModules {
        modules: Vec<String>,
        #[serde(default)]
        extensions: Option<Vec<String>>,
        #[serde(default)]
        filter: Option<String>,
    },
    GetFilesInModulesAsync {
        modules: Vec<String>,
        #[serde(default)]
        extensions: Option<Vec<String>>,
        #[serde(default)]
        filter: Option<String>,
    },
    GetClassesInModules {
        modules: Vec<String>,
        #[serde(default)]
        symbol_type: Option<String>,
    },
    GetClassesInModulesAsync {
        modules: Vec<String>,
        #[serde(default)]
        symbol_type: Option<String>,
    },
    FindSymbolInModule { module: String, symbol: String },
    SearchFilesInModules {
        modules: Vec<String>,
        filter: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    SearchFilesInModulesAsync {
        modules: Vec<String>,
        filter: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    SearchSymbolsInModules {
        modules: Vec<String>,
        #[serde(default)]
        symbol_type: Option<String>,
        filter: String,
        #[serde(default)]
        limit: Option<usize>,
    },

    // File queries.
    // 文件查询。
    SearchFiles { part: String },
    SearchFilesByPathPart { part: String },
    SearchFilesByPathPartAsync { part: String },
    GetDirectoriesInModule { module_id: i64 },
    GetModuleFilesByNameAndRoot { name: String, root: String },
    GetModuleDirsByNameAndRoot { name: String, root: String },
    GetDependFiles {
        file_path: String,
        #[serde(default)]
        recursive: bool,
        #[serde(default)]
        game_only: bool,
    },
    GetTargetFiles,
    GetAllFilePaths,
    GetAllFilesMetadata,

    // Symbol search and usage.
    // 符号搜索和引用查询。
    SearchSymbols { pattern: String, limit: usize },
    FindSymbolUsages {
        symbol_name: String,
        #[serde(default)]
        file_path: Option<String>,
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        line: Option<u32>,
        #[serde(default)]
        character: Option<u32>,
    },
    FindSymbolUsagesAsync {
        symbol_name: String,
        #[serde(default)]
        file_path: Option<String>,
    },

    // Buffer/navigation/completion.
    // 当前 buffer、跳转和补全。
    ParseBuffer {
        content: String,
        #[serde(default)]
        file_path: Option<String>,
        #[serde(default)]
        line: Option<u32>,
        #[serde(default)]
        character: Option<u32>,
    },
    GotoDefinition {
        content: String,
        line: u32,
        character: u32,
        #[serde(default)]
        file_path: Option<String>,
    },
    GotoImplementation {
        content: String,
        line: u32,
        character: u32,
        #[serde(default)]
        file_path: Option<String>,
    },
    GetCompletions {
        content: String,
        line: u32,
        character: u32,
        #[serde(default)]
        file_path: Option<String>,
    },
    GetDiagnostics {
        content: String,
        #[serde(default)]
        file_path: Option<String>,
    },
    ParseBuildDiagnostics {
        output: String,
    },

    // Asset queries.
    // 资产查询。
    GetAssets,
    GetAssetUsages { asset_path: String },
    GetAssetDependencies { asset_path: String },
    GrepAssets { pattern: String },

    // Config queries.
    // 配置查询。
    GetConfigData {
        #[serde(default)]
        engine_root: Option<String>,
    },

    // Legacy or future extension points.
    // 旧接口或未来扩展接口。
    LoadComponentData { component: String },
    GetProgramFiles,
    GetAllIniFiles,
    UpdateMemberReturnType {
        class_name: String,
        member_name: String,
        return_type: String,
    },
}

// -----------------------------------------------------------------------------
// Modify protocol
// -----------------------------------------------------------------------------

/// Request to add a module entry to .uproject or .uplugin.
/// 给 .uproject 或 .uplugin 添加模块的请求。
#[derive(Debug, Deserialize)]
pub struct ModifyUprojectAddModuleRequest {
    pub file_path: String,
    pub module_name: String,
    pub module_type: String,
    pub loading_phase: String,
}

/// Request to add a module entry to .Target.cs.
/// 给 .Target.cs 注册模块的请求。
#[derive(Debug, Deserialize)]
pub struct ModifyTargetAddModuleRequest {
    pub file_path: String,
    pub module_name: String,
}

/// Result returned by file modification RPC calls.
/// 文件修改 RPC 返回结果。
#[derive(Debug, Serialize)]
pub struct ModifyResult {
    pub success: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
