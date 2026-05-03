use anyhow::{anyhow, Result};
use rayon::prelude::*;
use serde_json::Value;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use tree_sitter::Query;
use u_scanner::types::{ParseResult, RawRequest, RefreshRequest, StdoutReporter};
use u_scanner::{db, refresh, scanner};

const DEFAULT_SERVER_PORT: u16 = 30110;
const READ_BUFFER_SIZE: usize = 4096;
const FRAME_HEADER_SIZE: usize = 4;
const DEFAULT_MSG_ID: u64 = 1;

/// CLI entry point.
/// CLI 主入口。
fn main() -> Result<()> {
    let args = CliArgs::from_env();
    let server_available = is_server_running(args.server_port);

    match args.command {
        Some(Command::Proxy { method, payload }) => {
            if server_available {
                return proxy_to_server(args.server_port, &method, &payload);
            }

            if method == "refresh" {
                return run_refresh_locally(&payload);
            }

            Err(anyhow!("Server is not running for command: {}", method))
        }

        Some(Command::ServerStatus { method }) => {
            if server_available {
                proxy_to_server(args.server_port, &method, "{}")
            } else {
                println!("Server not running.");
                Ok(())
            }
        }

        Some(Command::ScanStdin) | None => run_scan_from_stdin(args.server_port, server_available),
    }
}

// -----------------------------------------------------------------------------
// CLI args
// -----------------------------------------------------------------------------

/// Parsed CLI command.
/// 解析后的 CLI 命令。
enum Command {
    /// Proxy a JSON payload to the running server.
    /// 把 JSON payload 转发给正在运行的 server。
    Proxy { method: String, payload: String },

    /// Query server status-like commands.
    /// 查询 server 状态类命令。
    ServerStatus { method: String },

    /// Read scan request from stdin.
    /// 从 stdin 读取 scan 请求。
    ScanStdin,
}

/// Parsed CLI args.
/// 解析后的 CLI 参数。
struct CliArgs {
    server_port: u16,
    command: Option<Command>,
}

impl CliArgs {
    /// Parse command line args and UNL_SERVER_PORT.
    /// 解析命令行参数和 UNL_SERVER_PORT。
    fn from_env() -> Self {
        let args = std::env::args().collect::<Vec<_>>();
        let server_port = std::env::var("UNL_SERVER_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(DEFAULT_SERVER_PORT);

        let command = parse_command(&args).ok().flatten();

        Self {
            server_port,
            command,
        }
    }
}

/// Parse supported commands.
/// 解析支持的命令。
fn parse_command(args: &[String]) -> Result<Option<Command>> {
    let Some(cmd) = args.get(1) else {
        return Ok(None);
    };

    match cmd.as_str() {
        "refresh" | "watch" | "query" | "setup" => {
            let arg = args.get(2).ok_or_else(|| anyhow!("Missing JSON config or file path"))?;
            let payload = load_payload(arg)?;
            Ok(Some(Command::Proxy {
                method: cmd.clone(),
                payload,
            }))
        }

        "status" | "list_projects" => Ok(Some(Command::ServerStatus {
            method: cmd.clone(),
        })),

        "scan" => Ok(Some(Command::ScanStdin)),

        _ => Ok(Some(Command::ScanStdin)),
    }
}

/// Load JSON payload from inline JSON, file, or raw string.
/// 从内联 JSON、文件路径或原始字符串加载 JSON payload。
fn load_payload(input: &str) -> Result<String> {
    let trimmed = input.trim();

    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Ok(input.to_string());
    }

    if std::path::Path::new(input).exists() {
        return Ok(std::fs::read_to_string(input)?);
    }

    Ok(input.to_string())
}

// -----------------------------------------------------------------------------
// Server proxy
// -----------------------------------------------------------------------------

/// Check whether TCP server is reachable.
/// 检查 TCP server 是否可连接。
fn is_server_running(port: u16) -> bool {
    TcpStream::connect(server_addr(port)).is_ok()
}

/// Proxy one JSON request to running MsgPack server.
/// 把一个 JSON 请求代理到正在运行的 MsgPack server。
fn proxy_to_server(port: u16, method: &str, json_payload: &str) -> Result<()> {
    let params: Value = serde_json::from_str(json_payload)
        .map_err(|err| anyhow!("JSON parse error: {}", err))?;

    let mut stream = TcpStream::connect(server_addr(port))?;
    send_rpc_request(&mut stream, method, params)?;
    read_rpc_until_response(&mut stream)
}

/// Return server address string.
/// 返回 server 地址字符串。
fn server_addr(port: u16) -> String {
    format!("127.0.0.1:{}", port)
}

/// Send one RPC request frame.
/// 发送一个 RPC 请求帧。
fn send_rpc_request(stream: &mut TcpStream, method: &str, params: Value) -> Result<()> {
    let request = (0u32, DEFAULT_MSG_ID, method, params);
    let payload = rmp_serde::to_vec(&request)?;
    write_frame(stream, &payload)
}

/// Read frames until response is received.
/// 读取数据帧直到收到 response。
fn read_rpc_until_response(stream: &mut TcpStream) -> Result<()> {
    let mut buffer = Vec::new();
    let mut temp = [0u8; READ_BUFFER_SIZE];

    loop {
        let read = stream.read(&mut temp)?;

        if read == 0 {
            break;
        }

        buffer.extend_from_slice(&temp[..read]);

        while let Some(frame) = try_take_frame(&mut buffer)? {
            if handle_rpc_frame(&frame)? {
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Handle one MsgPack frame. Return true when final response is handled.
/// 处理一个 MsgPack 帧；处理到最终 response 时返回 true。
fn handle_rpc_frame(frame: &[u8]) -> Result<bool> {
    let msg = rmp_serde::from_slice::<Vec<Value>>(frame)?;

    let msg_type = msg.first().and_then(|value| value.as_u64()).unwrap_or(0);

    match msg_type {
        1 => {
            let err = msg.get(2).unwrap_or(&Value::Null);
            let result = msg.get(3).unwrap_or(&Value::Null);

            if !err.is_null() {
                return Err(anyhow!("Server error: {}", err));
            }

            println!("{}", serde_json::to_string_pretty(result)?);
            Ok(true)
        }

        2 => {
            handle_notification(&msg)?;
            Ok(false)
        }

        _ => Ok(false),
    }
}

/// Handle server notification frames.
/// 处理 server notification 帧。
fn handle_notification(_msg: &[Value]) -> Result<()> {
    // Progress notifications are intentionally ignored by default.
    // 默认忽略进度通知，避免 CLI 输出被刷屏。
    Ok(())
}

/// Write length-prefixed frame.
/// 写入带 4 字节长度前缀的数据帧。
fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    stream.write_all(&(payload.len() as u32).to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

/// Try to take one complete frame from buffer.
/// 尝试从缓冲区取出一个完整数据帧。
fn try_take_frame(buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    if buffer.len() < FRAME_HEADER_SIZE {
        return Ok(None);
    }

    let len = u32::from_be_bytes(buffer[..FRAME_HEADER_SIZE].try_into().unwrap()) as usize;

    if buffer.len() < FRAME_HEADER_SIZE + len {
        return Ok(None);
    }

    let frame = buffer[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + len].to_vec();
    buffer.drain(..FRAME_HEADER_SIZE + len);

    Ok(Some(frame))
}

// -----------------------------------------------------------------------------
// Local fallback
// -----------------------------------------------------------------------------

/// Run refresh locally when server is not available.
/// server 不可用时，本地执行 refresh。
fn run_refresh_locally(json_payload: &str) -> Result<()> {
    let request = serde_json::from_str::<RefreshRequest>(json_payload)?;
    refresh::run_refresh(request, Arc::new(StdoutReporter))
}

/// Read scan request from stdin and run or proxy it.
/// 从 stdin 读取 scan 请求并执行或代理。
fn run_scan_from_stdin(port: u16, server_available: bool) -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;

    if input.trim().is_empty() {
        return Ok(());
    }

    if server_available {
        return proxy_to_server(port, "scan", &input);
    }

    run_scan_locally(&input)
}

/// Run scan locally without server.
/// 不经过 server，直接本地扫描。
fn run_scan_locally(input: &str) -> Result<()> {
    let language = tree_sitter_unreal_cpp::LANGUAGE.into();
    let query = Arc::new(Query::new(&language, scanner::QUERY_STR)?);
    let include_query = Arc::new(Query::new(&language, scanner::INCLUDE_QUERY_STR)?);

    let request = serde_json::from_str::<RawRequest>(input)?;

    match request {
        RawRequest::Scan(req) => {
            let db_path = req.files.first().and_then(|file| file.db_path.clone());

            let results = req
                .files
                .into_par_iter()
                .map(|input_file| {
                    scanner::process_file(&input_file, &language, &query, &include_query)
                        .unwrap_or_else(|_| ParseResult {
                            path: input_file.path,
                            status: "error".to_string(),
                            mtime: input_file.mtime,
                            data: None,
                            module_id: input_file.module_id,
                        })
                })
                .collect::<Vec<_>>();

            if let Some(db_path) = db_path {
                if let Ok(mut conn) = rusqlite::Connection::open(db_path) {
                    let _ = db::save_to_db(
                        &mut conn,
                        &results,
                        Arc::new(StdoutReporter),
                    );
                }
            }

            print_parse_results(&results)?;
        }

        RawRequest::Refresh(req) => {
            refresh::run_refresh(req, Arc::new(StdoutReporter))?;
        }
    }

    Ok(())
}

/// Print parse results as JSON lines.
/// 以 JSON Lines 形式输出解析结果。
fn print_parse_results(results: &[ParseResult]) -> Result<()> {
    let mut stdout = io::stdout().lock();

    for result in results {
        let json = serde_json::to_string(result)?;
        stdout.write_all(json.as_bytes())?;
        stdout.write_all(b"\n")?;
    }

    stdout.flush()?;
    Ok(())
}
