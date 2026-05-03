//! UCore scanner/server library entry.
//! UCore 扫描器和 server 的 crate 根入口。
//!
//! This file only wires modules together.
//! 这个文件只负责组织模块，不放具体业务逻辑。

pub mod completion;
pub mod db;
pub mod diagnostics;
pub mod edit;
pub mod parser;
pub mod query;
pub mod refresh;
pub mod server;
pub mod types;
pub mod uasset;

pub mod scanner {
    pub use super::parser::cpp::*;
}
