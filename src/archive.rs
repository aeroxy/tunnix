//! Streaming tar + zstd bridge shared by the client (`transfer`) and server.
//!
//! `tar` and `zstd` are synchronous `Read`/`Write` types, so the archive is
//! built / unpacked on a `spawn_blocking` thread and bridged to the async world
//! over an `mpsc` channel. This keeps memory bounded (we never hold the whole
//! archive) and lets the channel's bounded capacity apply backpressure between
//! the blocking compressor and the network.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Channel capacity (number of in-flight chunks) for both directions. Bounded so
/// a slow network throttles the blocking compress/decompress thread.
const CHANNEL_CAP: usize = 256;

/// `std::io::Write` sink that ships each written buffer into an async channel.
/// Used as the zstd encoder's output.
struct ChannelWriter {
    tx: mpsc::Sender<Vec<u8>>,
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Never enqueue an empty chunk: the reader treats an empty `Vec` as EOF,
        // so a zero-length write must not reach the channel.
        if buf.is_empty() {
            return Ok(0);
        }
        // `blocking_send` is valid here because the writer only ever runs on a
        // `spawn_blocking` thread, never on the async runtime.
        self.tx
            .blocking_send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "transfer channel closed"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `std::io::Read` source that pulls buffers from an async channel. Used as the
/// zstd decoder's input; reaching a dropped sender signals EOF.
struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    /// Leftover bytes from the last chunk that didn't fit the caller's buffer.
    leftover: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // A zero-length read must return immediately without blocking on the
        // channel, per the `Read` contract.
        if buf.is_empty() {
            return Ok(0);
        }
        // Loop so a transient empty chunk is skipped rather than mistaken for
        // EOF: only a dropped sender (`None`) ends the stream.
        loop {
            if self.pos < self.leftover.len() {
                let n = std::cmp::min(buf.len(), self.leftover.len() - self.pos);
                buf[..n].copy_from_slice(&self.leftover[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.rx.blocking_recv() {
                Some(chunk) => {
                    self.leftover = chunk;
                    self.pos = 0;
                }
                None => return Ok(0), // sender dropped => EOF
            }
        }
    }
}

/// Pick the archive root name for `src` — its basename, so `push ./foo /remote`
/// lands as `/remote/foo/...`. Falls back to the canonical basename, then to
/// `"archive"` for path-less inputs like `.`.
fn archive_root_name(src: &Path) -> OsString {
    src.file_name()
        .map(|s| s.to_os_string())
        .or_else(|| {
            src.canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_os_string()))
        })
        .unwrap_or_else(|| OsString::from("archive"))
}

/// Spawn a blocking task that tars + zstd-compresses each path in `srcs` (files
/// or directories) into a single archive and streams the bytes out over the
/// returned channel. Each source is added under its own basename, so multiple
/// roots coexist in one archive (sources sharing a basename collide — the later
/// one wins on unpack). The `JoinHandle` resolves once the whole archive has
/// been written (or with an error).
pub(crate) fn spawn_compress(
    srcs: Vec<PathBuf>,
    level: i32,
) -> (mpsc::Receiver<Vec<u8>>, JoinHandle<Result<()>>) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAP);
    let handle = tokio::task::spawn_blocking(move || -> Result<()> {
        let writer = ChannelWriter { tx };
        let encoder = zstd::stream::Encoder::new(writer, level)
            .context("init zstd encoder")?;
        let mut builder = tar::Builder::new(encoder);
        builder.follow_symlinks(false);

        for src in &srcs {
            // symlink_metadata (not metadata) so a symlinked root isn't
            // dereferenced — matches builder.follow_symlinks(false), letting
            // symlinks (including broken ones) be archived as links.
            let meta = std::fs::symlink_metadata(src)
                .with_context(|| format!("cannot read {}", src.display()))?;
            let name = archive_root_name(src);
            if meta.is_dir() {
                builder
                    .append_dir_all(&name, src)
                    .with_context(|| format!("archive directory {}", src.display()))?;
            } else {
                builder
                    .append_path_with_name(src, &name)
                    .with_context(|| format!("archive file {}", src.display()))?;
            }
        }

        // into_inner() finalizes the tar (trailing zero blocks) and returns the
        // encoder; finish() writes the zstd epilogue. Dropping the writer
        // afterwards closes the channel, signalling EOF to the reader.
        let encoder = builder.into_inner().context("finalize tar")?;
        encoder.finish().context("finalize zstd")?;
        Ok(())
    });
    (rx, handle)
}

/// Spawn a blocking task that consumes a zstd-compressed tar stream from the
/// returned channel and unpacks it into `dest`. Dropping the sender ends the
/// stream; the `JoinHandle` then resolves with the unpack result.
pub(crate) fn spawn_decompress(dest: PathBuf) -> (mpsc::Sender<Vec<u8>>, JoinHandle<Result<()>>) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAP);
    let handle = tokio::task::spawn_blocking(move || -> Result<()> {
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("create destination {}", dest.display()))?;
        let reader = ChannelReader { rx, leftover: Vec::new(), pos: 0 };
        let decoder = zstd::stream::Decoder::new(reader).context("init zstd decoder")?;
        let mut archive = tar::Archive::new(decoder);
        // tar's unpack guards against path traversal: entries with `..` or
        // absolute paths that escape `dest` are skipped rather than written out.
        archive
            .unpack(&dest)
            .with_context(|| format!("unpack into {}", dest.display()))?;
        Ok(())
    });
    (tx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_directory_tree() {
        let base = std::env::temp_dir().join(format!("tunnix-archive-test-{}", std::process::id()));
        let src = base.join("src");
        let out = base.join("out");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(src.join("nested")).unwrap();

        std::fs::write(src.join("a.txt"), b"hello world").unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(src.join("nested/big.bin"), &big).unwrap();
        // An executable file, to check permissions survive on unix.
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::PermissionsExt;
            let exe = src.join("run.sh");
            let mut f = std::fs::File::create(&exe).unwrap();
            f.write_all(b"#!/bin/sh\necho hi\n").unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Compress src, pump chunks straight into a decompressor targeting out.
        let (mut chunks, comp) = spawn_compress(vec![src.clone()], 3);
        let (sink, decomp) = spawn_decompress(out.clone());
        while let Some(c) = chunks.recv().await {
            sink.send(c).await.unwrap();
        }
        drop(sink);
        comp.await.unwrap().unwrap();
        decomp.await.unwrap().unwrap();

        // The archive root is the basename of src ("src").
        let root = out.join("src");
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"hello world");
        assert_eq!(std::fs::read(root.join("nested/big.bin")).unwrap(), big);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("run.sh")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }

        let _ = std::fs::remove_dir_all(&base);
    }
}
