//! Length-prefixed postcard frames: a big-endian `u32` length, then the body.
//!
//! The peer is untrusted until paired (and the target stays untrusted after),
//! so readers always pass an explicit size limit.

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Errors while reading or writing frames.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {size} bytes exceeds the limit of {limit} bytes")]
    TooLarge { size: usize, limit: usize },
    #[error("malformed frame: {0}")]
    Decode(#[from] postcard::Error),
}

/// Writes one frame and flushes.
///
/// # Errors
///
/// If the value doesn't serialise, is larger than 4 GiB, or writing fails.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = postcard::to_allocvec(value)?;
    let size = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge {
        size: body.len(),
        limit: u32::MAX as usize,
    })?;
    writer.write_all(&size.to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one frame of at most `limit` bytes.
///
/// # Errors
///
/// If reading fails, the frame is larger than `limit`, or it doesn't
/// deserialise as `T`.
pub async fn read_frame<R, T>(reader: &mut R, limit: usize) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let size = reader.read_u32().await? as usize;
    if size > limit {
        return Err(FrameError::TooLarge { size, limit });
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body).await?;
    Ok(postcard::from_bytes(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_and_limit() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, &("hello", 42u32)).await.unwrap();
        let got: (String, u32) = read_frame(&mut b, 64).await.unwrap();
        assert_eq!(got, ("hello".to_string(), 42));

        write_frame(&mut a, &vec![0u8; 100]).await.unwrap();
        let err = read_frame::<_, Vec<u8>>(&mut b, 64).await.unwrap_err();
        assert!(
            matches!(err, FrameError::TooLarge { limit: 64, .. }),
            "{err}"
        );
    }
}
