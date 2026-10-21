use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

/// Keep the reverse direction alive after EOF, and expire only when neither
/// direction makes progress. Both copies belong to this future so cancellation
/// also releases the sockets and streams.
pub(super) async fn relay<R, W, S>(
    reader: R,
    writer: W,
    remote: S,
    idle_timeout: Duration,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (remote_reader, remote_writer) = tokio::io::split(remote);
    // Last-progress timestamp encoded as "milliseconds since `base` + 1".
    // A plain atomic keeps the per-chunk cost to one store; the previous
    // `watch` channel took an internal lock on every read and write.
    let base = Instant::now();
    let activity = AtomicU64::new(1);
    let copy = async {
        tokio::try_join!(
            copy_direction(reader, remote_writer, &activity, base),
            copy_direction(remote_reader, writer, &activity, base),
        )?;
        Ok(())
    };
    let idle = async {
        let mut last = activity.load(Ordering::Relaxed);
        loop {
            let deadline = base + Duration::from_millis(last) + idle_timeout;
            tokio::time::sleep_until(deadline).await;
            let current = activity.load(Ordering::Relaxed);
            if current == last {
                break;
            }
            last = current;
        }
    };

    tokio::select! {
        result = copy => result,
        _ = idle => Err(io::Error::new(io::ErrorKind::TimedOut, "TCP relay idle timeout")),
    }
}

async fn copy_direction<R, W>(
    mut reader: R,
    mut writer: W,
    activity: &AtomicU64,
    base: Instant,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0; 16 * 1024];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return writer.shutdown().await;
        }
        activity.store(base.elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
        let mut remaining = &buf[..n];
        while !remaining.is_empty() {
            let written = writer.write(remaining).await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            activity.store(base.elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
            remaining = &remaining[written..];
        }
    }
}

#[cfg(test)]
mod tests;
