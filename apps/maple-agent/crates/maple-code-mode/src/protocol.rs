use crate::{FINAL_BYTES, MAX_FRAME_BYTES, MAX_OUTPUT_CHUNK_BYTES};
use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}
impl Stream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum HostFrame {
    Execute {
        generation: u64,
        execution_id: u64,
        code: String,
    },
    Shutdown {
        generation: u64,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WorkerFrame {
    Ready {
        generation: u64,
        protocol_version: u32,
        implementation: String,
        version: String,
        executable: String,
        cwd: String,
    },
    Output {
        generation: u64,
        #[serde(deserialize_with = "required_execution_id")]
        execution_id: Option<u64>,
        stream: Stream,
        text: String,
    },
    Result {
        generation: u64,
        execution_id: u64,
        text: String,
    },
    Error {
        generation: u64,
        execution_id: u64,
        traceback: String,
    },
    Done {
        generation: u64,
        execution_id: u64,
        status: DoneStatus,
        elapsed_ms: u64,
        dropped_stdout_bytes: u64,
        dropped_stderr_bytes: u64,
    },
    Fatal {
        generation: u64,
        message: String,
    },
}
fn required_execution_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    Option::<u64>::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DoneStatus {
    Ok,
    Error,
    Cancelled,
}
impl WorkerFrame {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Ready { generation, .. }
            | Self::Output { generation, .. }
            | Self::Result { generation, .. }
            | Self::Error { generation, .. }
            | Self::Done { generation, .. }
            | Self::Fatal { generation, .. } => *generation,
        }
    }
    fn validate_bounds(&self) -> io::Result<()> {
        let valid = match self {
            Self::Output { text, .. } => text.len() <= MAX_OUTPUT_CHUNK_BYTES,
            Self::Result { text, .. } => text.len() <= FINAL_BYTES,
            Self::Error { traceback, .. } => traceback.len() <= FINAL_BYTES,
            Self::Fatal { message, .. } => message.len() <= FINAL_BYTES,
            Self::Ready {
                implementation,
                version,
                executable,
                cwd,
                ..
            } => {
                implementation.len() <= 32
                    && version.len() <= 32
                    && executable.len() <= 16 * 1024
                    && cwd.len() <= 16 * 1024
            }
            Self::Done { .. } => true,
        };
        if valid {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Python frame field exceeds its byte limit",
            ))
        }
    }
}

pub(crate) fn encode(frame: &HostFrame) -> io::Result<Vec<u8>> {
    let encoded = serde_json::to_vec(frame).map_err(io::Error::other)?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Encoded Python request exceeds 1 MiB",
        ));
    }
    Ok(encoded)
}
pub(crate) async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &HostFrame,
) -> io::Result<()> {
    let encoded = encode(frame)?;
    writer.write_u32(encoded.len() as u32).await?;
    writer.write_all(&encoded).await?;
    writer.flush().await
}
pub(crate) async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<WorkerFrame> {
    let size = reader.read_u32().await? as usize;
    if size == 0 || size > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Python frame length outside 1..=1 MiB",
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    let frame: WorkerFrame = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Invalid Python protocol frame: {error}"),
        )
    })?;
    frame.validate_bounds()?;
    Ok(frame)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rejects_length_before_reading_payload() {
        let bytes = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes();
        assert_eq!(
            read_frame(&mut &bytes[..]).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[tokio::test]
    async fn rejects_wrong_direction_and_invalid_utf8() {
        for bytes in [br#"{"type":"shutdown","generation":1}"#.to_vec(), vec![255]] {
            let mut framed = (bytes.len() as u32).to_be_bytes().to_vec();
            framed.extend(bytes);
            assert!(read_frame(&mut &framed[..]).await.is_err());
        }
    }
    #[tokio::test]
    async fn output_requires_explicit_attribution_and_respects_chunk_bounds() {
        for frame in [
            serde_json::json!({"type":"output","generation":1,"stream":"stdout","text":"missing identity"}),
            serde_json::json!({"type":"output","generation":1,"execution_id":null,"stream":"stdout","text":"x".repeat(MAX_OUTPUT_CHUNK_BYTES + 1)}),
        ] {
            let bytes = serde_json::to_vec(&frame).unwrap();
            let mut encoded = (bytes.len() as u32).to_be_bytes().to_vec();
            encoded.extend(bytes);
            assert!(read_frame(&mut &encoded[..]).await.is_err());
        }
    }
}
