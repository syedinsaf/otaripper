use anyhow::{Context, bail};
use console::Style;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

const CACHE_SIZE: u64 = 256 * 1024; // 256KB head/tail ZIP pre fetch

// global bandwidth tracker (shows exactly how much data we saved the user)
pub static NETWORK_BYTES_READ: AtomicUsize = AtomicUsize::new(0);

// thread safe dynamic UI controller
pub static GLOBAL_NETWORK_STATUS: OnceLock<ProgressBar> = OnceLock::new();
// Used as a flag, not a true count, only the 0→1 and >0→0 transitions update the UI
static OFFLINE_THREADS: AtomicUsize = AtomicUsize::new(0);

pub fn set_network_offline(msg: &str) {
    if let Some(pb) = GLOBAL_NETWORK_STATUS.get() {
        let style = ProgressStyle::with_template("\n{prefix:.cyan.bold} {msg}").unwrap();
        pb.set_style(style);
        pb.set_message(msg.to_string());
        pb.reset_eta();
    }
}

pub fn set_network_online() {
    if let Some(pb) = GLOBAL_NETWORK_STATUS.get() {
        let style = ProgressStyle::with_template(
            "\n{prefix:.cyan.bold} {bytes}/{total_bytes} ({bytes_per_sec}, ETA: {eta})",
        )
        .unwrap();
        pb.set_style(style);
        pb.set_message("");
        pb.reset_eta();
    }
}

fn mark_offline(msg: &str) {
    let count = OFFLINE_THREADS.fetch_add(1, Ordering::SeqCst);
    if count == 0 {
        let styled = Style::new().bold().yellow().apply_to(msg).to_string();
        set_network_offline(&styled);
    }
}

/// pulls a specific partition chunk over HTTP with aggressive retries
/// Fakes the progress bar bytes so it matches the uncompressed disk writes (keeps UI smooth)
pub fn fetch_http_chunk(
    client: &reqwest::blocking::Client,
    url: &str,
    start: u64,
    size: usize,
    pb: &ProgressBar,
    total_dst_size: usize,
) -> anyhow::Result<Vec<u8>> {
    // prevent u64 underflow panic on 0 byte operations
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; size];
    let ratio = total_dst_size as f64 / size as f64;

    // Parallel chunking For heavy payloads, we split the single HTTP request into
    // multiple 8MB parallel Range requests to saturate the bandwidth!
    let part_size = 8 * 1024 * 1024;

    if size > part_size {
        let results: Vec<anyhow::Result<()>> = buf
            .par_chunks_mut(part_size)
            .enumerate()
            .map(|(i, chunk_slice)| {
                let chunk_start = start + (i * part_size) as u64;
                let chunk_end = chunk_start + chunk_slice.len() as u64 - 1;
                fetch_range_with_retries(
                    client,
                    url,
                    chunk_start,
                    chunk_end,
                    chunk_slice,
                    pb,
                    ratio,
                )
            })
            .collect();

        for res in results {
            res?;
        }
    } else {
        // normal fast path for smaller operations like where the overhead of parallelism isn't worth it
        fetch_range_with_retries(
            client,
            url,
            start,
            start + size as u64 - 1,
            &mut buf,
            pb,
            ratio,
        )?;
    }

    Ok(buf)
}

fn fetch_range_with_retries(
    client: &reqwest::blocking::Client,
    url: &str,
    start: u64,
    end: u64,
    buf: &mut [u8],
    pb: &ProgressBar,
    ratio: f64,
) -> anyhow::Result<()> {
    let size = buf.len();
    let mut total_read = 0;
    let mut ui_reported = 0u64;
    // no retry cap, means loops until done or Ctrl+C, backoff starts at 500ms and tops out at 3s
    let mut backoff_ms = 500;

    // massive fr 256KB read buffer to prevent loop/syscall overhead
    let mut chunk = vec![0u8; 256 * 1024];

    while total_read < size {
        let req_start = start + total_read as u64;

        match client
            .get(url)
            .header("Range", format!("bytes={}-{}", req_start, end))
            .send()
        {
            Ok(mut resp) => {
                // check If the server returns 200 OK, it ignored our Range request
                // and is trying to send the entire ROM, Bail gracefully!
                if resp.status() == reqwest::StatusCode::OK {
                    bail!(
                        "The remote server does not support HTTP Range requests, Streaming is impossible!"
                    );
                } else if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    // 429 rate limit, back off and retry
                    mark_offline("server is busy, pausing for a few seconds...");
                    backoff_ms = 500;
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    continue;
                } else if !resp.status().is_success() {
                    bail!("Server rejected range request: {}", resp.status());
                } else {
                    backoff_ms = 500;
                    let was_offline = OFFLINE_THREADS.swap(0, Ordering::SeqCst);
                    if was_offline > 0 {
                        set_network_online();
                    }

                    loop {
                        let n = match resp.read(&mut chunk) {
                            Ok(0) => break, // EOF
                            Ok(n) => n,
                            Err(_) => {
                                mark_offline("connection lost, waiting to resume...");
                                break;
                            }
                        };

                        // prevent buffer overflows if a rogue server sends too much data
                        if total_read + n > size {
                            bail!(
                                "Rogue server sent more data than requested, Aborting to prevent memory corruption!"
                            );
                        }

                        buf[total_read..total_read + n].copy_from_slice(&chunk[..n]);
                        total_read += n;
                        NETWORK_BYTES_READ.fetch_add(n, Ordering::Relaxed);
                        if let Some(monitor) = GLOBAL_NETWORK_STATUS.get() {
                            monitor.inc(n as u64);
                        }

                        // Sync UI in real time
                        let expected = (total_read as f64 * ratio) as u64;
                        let diff = expected.saturating_sub(ui_reported);
                        if diff > 0 {
                            pb.inc(diff);
                            ui_reported += diff;
                        }
                    }
                }
            }
            Err(_) => {
                mark_offline("connection lost, waiting to resume...");
            }
        }

        if total_read < size {
            std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
            backoff_ms = (backoff_ms * 2).min(3_000); // cap offline sleep to 3s for fast reconnects
        }
    }
    Ok(())
}

/// instant start HTTP reader, that pre-fetches the head and tail of the remote ZIP
pub struct CachingHttpReader {
    client: reqwest::blocking::Client,
    url: String,
    length: u64,
    pos: u64,
    head_buf: Vec<u8>,
    tail_buf: Vec<u8>,
    tail_start: u64,
}

impl CachingHttpReader {
    pub fn new(
        client: reqwest::blocking::Client,
        url: &str,
        pb: &ProgressBar,
    ) -> anyhow::Result<Self> {
        pb.set_message("Connecting to remote server...");
        // some CDNs and pre-signed URLs (OSS, S3, GCS) reject HEAD so a 0-byte range GET works everywhere
        let resp = client.get(url).header("Range", "bytes=0-0").send()?;
        if !resp.status().is_success() {
            bail!("Failed to access URL: {}", resp.status());
        }

        // prefer Content-Range for the real size, fall back to Content-Length
        let length = resp
            .headers()
            .get("Content-Range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split('/').next_back())
            .and_then(|v| v.parse::<u64>().ok())
            .or_else(|| {
                resp.headers()
                    .get("Content-Length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .context("Server didn't return file size, can't seek this remote file.")?;

        let head_size = CACHE_SIZE.min(length);
        let tail_size = CACHE_SIZE.min(length);
        let tail_start = length.saturating_sub(tail_size).max(head_size);

        // upgrade the spinner to show realtime byte progress (makes it feel more responsive)
        pb.set_length(head_size + tail_size);
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan.bold} {msg} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec})")
                .unwrap()
                .progress_chars("=> ")
        );

        // first, Snag the Payload headers (Head)
        pb.set_message("Fetching headers...");
        let mut head_buf = Vec::with_capacity(head_size as usize);
        if head_size > 0 {
            loop {
                head_buf.clear();
                match client
                    .get(url)
                    .header("Range", format!("bytes=0-{}", head_size - 1))
                    .send()
                {
                    Ok(mut req) => {
                        if req.status() == reqwest::StatusCode::OK {
                            bail!(
                                "The remote server does not support HTTP Range requests, Streaming is impossible!"
                            );
                        } else if req.status().is_success() {
                            pb.set_message("Fetching headers...");
                            let mut chunk = vec![0u8; 128 * 1024]; // 128kb burst read
                            loop {
                                let n = match req.read(&mut chunk) {
                                    Ok(0) => break,
                                    Ok(n) => n,
                                    Err(_) => {
                                        pb.set_message(
                                            console::Style::new()
                                                .bold()
                                                .yellow()
                                                .apply_to("connection lost, waiting to resume...")
                                                .to_string(),
                                        );
                                        break;
                                    }
                                };
                                head_buf.extend_from_slice(&chunk[..n]);
                                NETWORK_BYTES_READ.fetch_add(n, Ordering::Relaxed);
                                pb.inc(n as u64);
                            }
                        }
                        if head_buf.len() >= head_size as usize {
                            break;
                        }
                    }
                    Err(_) => {
                        pb.set_message(
                            console::Style::new()
                                .bold()
                                .yellow()
                                .apply_to("connection lost, waiting to resume...")
                                .to_string(),
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }

        // second, Snag the Zip Central Directory (Tail)
        pb.set_message("Fetching ZIP directory...");
        let mut tail_buf = Vec::with_capacity(tail_size as usize);
        if tail_start < length {
            loop {
                tail_buf.clear();
                match client
                    .get(url)
                    .header("Range", format!("bytes={}-{}", tail_start, length - 1))
                    .send()
                {
                    Ok(mut req) => {
                        if req.status() == reqwest::StatusCode::OK {
                            bail!(
                                "The remote server does not support HTTP Range requests, Streaming is impossible!"
                            );
                        } else if req.status().is_success() {
                            pb.set_message("Fetching ZIP directory...");
                            let mut chunk = vec![0u8; 128 * 1024];
                            loop {
                                let n = match req.read(&mut chunk) {
                                    Ok(0) => break,
                                    Ok(n) => n,
                                    Err(_) => {
                                        pb.set_message(
                                            console::Style::new()
                                                .bold()
                                                .yellow()
                                                .apply_to("connection lost, waiting to resume...")
                                                .to_string(),
                                        );
                                        break;
                                    }
                                };
                                tail_buf.extend_from_slice(&chunk[..n]);
                                NETWORK_BYTES_READ.fetch_add(n, Ordering::Relaxed);
                                pb.inc(n as u64);
                            }
                        }
                        if tail_buf.len() >= tail_size as usize {
                            break;
                        }
                    }
                    Err(_) => {
                        pb.set_message(
                            console::Style::new()
                                .bold()
                                .yellow()
                                .apply_to("connection lost, waiting to resume...")
                                .to_string(),
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }

        // Snap tail_start down if head buffer was actually smaller than we think
        let actual_tail_start = length.saturating_sub(tail_size).max(head_buf.len() as u64);

        Ok(Self {
            client,
            url: url.to_string(),
            length,
            pos: 0,
            head_buf,
            tail_buf,
            tail_start: actual_tail_start,
        })
    }
}

impl Read for CachingHttpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.length || buf.is_empty() {
            return Ok(0);
        }

        let end = (self.pos + buf.len() as u64 - 1).min(self.length - 1);
        let len_to_copy = (end - self.pos + 1) as usize;

        // Head cache hit...
        if self.pos < self.head_buf.len() as u64 && end < self.head_buf.len() as u64 {
            let p = self.pos as usize;
            buf[..len_to_copy].copy_from_slice(&self.head_buf[p..p + len_to_copy]);
            self.pos += len_to_copy as u64;
            return Ok(len_to_copy);
        }

        // Tail cache hit...
        if self.pos >= self.tail_start && end < self.length {
            let offset = (self.pos - self.tail_start) as usize;
            buf[..len_to_copy].copy_from_slice(&self.tail_buf[offset..offset + len_to_copy]);
            self.pos += len_to_copy as u64;
            return Ok(len_to_copy);
        }

        // cache miss --> Network roundtrip
        let mut resp = self
            .client
            .get(&self.url)
            .header("Range", format!("bytes={}-{}", self.pos, end))
            .send()
            .map_err(io::Error::other)?;

        let n = resp.read(buf)?;
        NETWORK_BYTES_READ.fetch_add(n, Ordering::Relaxed);

        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for CachingHttpReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(p) => self.length as i128 + p as i128,
            SeekFrom::Current(p) => self.pos as i128 + p as i128,
        };

        if new_pos < 0 || new_pos > u64::MAX as i128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek position (offset out of bounds or negative)",
            ));
        }

        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_caching_http_reader_seek() {
        let mut reader = CachingHttpReader {
            client: reqwest::blocking::Client::new(),
            url: "http://localhost/test.zip".to_string(),
            length: 1000,
            pos: 100,
            head_buf: vec![],
            tail_buf: vec![],
            tail_start: 1000,
        };

        // SeekFrom::Start
        assert_eq!(reader.seek(SeekFrom::Start(500)).unwrap(), 500);

        // SeekFrom::Current
        assert_eq!(reader.seek(SeekFrom::Current(50)).unwrap(), 550);

        // SeekFrom::End
        assert_eq!(reader.seek(SeekFrom::End(-100)).unwrap(), 900);

        // Invalid negative seek
        assert!(reader.seek(SeekFrom::End(-2000)).is_err());
        assert!(reader.seek(SeekFrom::Current(-5000)).is_err());
    }
}
