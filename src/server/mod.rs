pub mod asset;
pub mod handlers;
pub mod state;
pub mod utils;
pub mod watcher;

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::server::state::AppState;

const WRITE_CHANNEL_SIZE: usize = 2000;
const READ_BUFFER_SIZE: usize = 8192;
const FRAME_HEADER_SIZE: usize = 4;
const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// Handle one TCP client connection.
/// 处理一个 TCP 客户端连接。
pub async fn handle_connection(socket: TcpStream, state: Arc<AppState>) {
    let peer = socket.peer_addr().ok();

    info!("Client connected: {:?}", peer);

    let (read_half, write_half) = socket.into_split();
    let (tx, rx) = mpsc::channel::<Vec<u8>>(WRITE_CHANNEL_SIZE);

    let writer = tokio::spawn(writer_loop(write_half, rx));
    let reader = reader_loop(read_half, state, tx).await;

    if let Err(err) = reader {
        warn!("Client reader stopped with error: {}", err);
    }

    writer.abort();

    info!("Client disconnected: {:?}", peer);
}

// -----------------------------------------------------------------------------
// Reader/writer loops
// -----------------------------------------------------------------------------

/// Read length-prefixed MessagePack frames and dispatch RPC requests.
/// 读取带长度前缀的 MessagePack 数据帧，并分发 RPC 请求。
async fn reader_loop(
    mut read_half: tokio::net::tcp::OwnedReadHalf,
    state: Arc<AppState>,
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let mut buffer = Vec::new();
    let mut temp = [0u8; READ_BUFFER_SIZE];

    loop {
        let read = read_half.read(&mut temp).await?;

        if read == 0 {
            break;
        }

        buffer.extend_from_slice(&temp[..read]);

        while let Some(frame) = try_take_frame(&mut buffer)? {
            let state = state.clone();
            let tx = tx.clone();

            tokio::spawn(async move {
                if let Err(err) = process_frame(frame, state, tx).await {
                    error!("Failed to process RPC frame: {}", err);
                }
            });
        }
    }

    Ok(())
}

/// Write encoded response frames to the socket.
/// 把编码后的响应帧写回 socket。
async fn writer_loop(
    mut write_half: tokio::net::tcp::OwnedWriteHalf,
    mut rx: mpsc::Receiver<Vec<u8>>,
) {
    while let Some(data) = rx.recv().await {
        if let Err(err) = write_half.write_all(&data).await {
            warn!("RPC write failed: {}", err);
            break;
        }

        if let Err(err) = write_half.flush().await {
            warn!("RPC flush failed: {}", err);
            break;
        }
    }
}

/// Try to extract one complete frame from the read buffer.
/// 尝试从读取缓冲区中取出一个完整数据帧。
fn try_take_frame(buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    if buffer.len() < FRAME_HEADER_SIZE {
        return Ok(None);
    }

    let len = u32::from_be_bytes(buffer[..FRAME_HEADER_SIZE].try_into().unwrap()) as usize;

    if len > MAX_FRAME_SIZE {
        return Err(anyhow!("RPC frame too large: {} bytes", len));
    }

    if buffer.len() < FRAME_HEADER_SIZE + len {
        return Ok(None);
    }

    let frame = buffer[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + len].to_vec();
    buffer.drain(..FRAME_HEADER_SIZE + len);

    Ok(Some(frame))
}

// -----------------------------------------------------------------------------
// RPC processing
// -----------------------------------------------------------------------------

type RpcRequest = (u32, u64, String, Value);
type RpcResponse = (u32, u64, Value, Value);

/// Decode and process one raw MessagePack frame.
/// 解码并处理一个原始 MessagePack 数据帧。
async fn process_frame(frame: Vec<u8>, state: Arc<AppState>, tx: mpsc::Sender<Vec<u8>>) -> Result<()> {
    let (_msg_type, msgid, method, params): RpcRequest = rmp_serde::from_slice(&frame)?;

    debug!("RPC request: method={}, msgid={}", method, msgid);

    let result = dispatch_rpc(msgid, &method, params.clone(), state, tx.clone()).await;
    let response = make_response(msgid, result, &method, &params);

    send_response(&tx, response).await?;

    Ok(())
}

/// Dispatch one RPC method to handlers.
/// 把一个 RPC method 分发到 handlers。
async fn dispatch_rpc(
    msgid: u64,
    method: &str,
    params: Value,
    state: Arc<AppState>,
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<Value> {
    match method {
        "ping" => handlers::handle_ping(&state, &params).await,
        "setup" => handlers::handle_setup(state.clone(), &params).await,
        "refresh" => handlers::handle_refresh(&state, &params, tx.clone()).await,
        "watch" => handlers::handle_watch(&state, &params).await,
        "query" => handlers::handle_query(state.clone(), &params, tx.clone(), msgid).await,
        "scan" => handlers::handle_scan(&state, &params).await,
        "status" => handlers::get_status(&state).await,
        "list_projects" => handlers::list_projects(&state).await,
        "delete_project" => handlers::handle_delete_project(&state, &params).await,

        "modify_uproject_add_module" => {
            handlers::handle_modify_uproject_add_module(&params).await
        }

        "modify_target_add_module" => {
            handlers::handle_modify_target_add_module(&params).await
        }

        other => Err(anyhow!("Unknown RPC method: {}", other)),
    }
}

/// Convert a handler result into an RPC response tuple.
/// 把 handler 结果转换成 RPC response 元组。
fn make_response(
    msgid: u64,
    result: Result<Value>,
    method: &str,
    params: &Value,
) -> RpcResponse {
    match result {
        Ok(value) => (1, msgid, Value::Null, value),

        Err(err) => {
            error!("RPC method failed: method={}, error={}, params={}", method, err, params);
            (1, msgid, Value::String(err.to_string()), Value::Null)
        }
    }
}

/// Encode and send one RPC response.
/// 编码并发送一个 RPC response。
async fn send_response(tx: &mpsc::Sender<Vec<u8>>, response: RpcResponse) -> Result<()> {
    let payload = rmp_serde::to_vec(&response)?;

    let mut framed = Vec::with_capacity(payload.len() + FRAME_HEADER_SIZE);
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(&payload);

    tx.send(framed).await?;
    Ok(())
}
