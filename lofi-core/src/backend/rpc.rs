use lofi_error::{Error, Result};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Child to Lofi. Guards a hostile or broken child.
pub(super) const MAX_READ_FRAME_BYTES: u64 = 4 * 1024 * 1024;
/// Lofi to child. Must carry the request image budget after base64 inflation.
pub(super) const MAX_WRITE_FRAME_BYTES: u64 = 16 * 1024 * 1024;

pub(super) async fn read(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Value> {
    let value = read_json(reader).await?;
    if value["jsonrpc"] != "2.0" {
        return Err(Error::State("invalid backend JSON-RPC envelope".into()));
    }
    Ok(value)
}

pub(super) async fn read_json(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Value> {
    let mut bytes = Vec::new();
    let count = reader
        .take(MAX_READ_FRAME_BYTES + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    if count == 0 {
        return Err(Error::State("backend disconnected".into()));
    }
    if count as u64 > MAX_READ_FRAME_BYTES || bytes.last() != Some(&b'\n') {
        return Err(Error::State(
            "backend frame exceeds limit or is incomplete".into(),
        ));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::State(format!("invalid backend JSON: {e}")))?;
    if !value.is_object() {
        return Err(Error::State("invalid backend JSON object".into()));
    }
    Ok(value)
}

pub(super) async fn write(writer: &mut (impl AsyncWrite + Unpin), value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| Error::State(e.to_string()))?;
    if bytes.len() as u64 >= MAX_WRITE_FRAME_BYTES {
        return Err(Error::State("backend frame exceeds limit".into()));
    }
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub(super) fn error(id: &Value, code: i32, message: &str) -> Value {
    serde_json::json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message}})
}
