//! A tiny HTTP server on 127.0.0.1 that streams the open video to the window.
//!
//! WebKitGTK plays video through GStreamer, which can't read Tauri's custom
//! URL schemes but handles plain http (with range requests for seeking) well.
//! Only files kroppi opened are served, each under a random token.

use std::{
    collections::HashMap,
    fs::File,
    hash::{BuildHasher, Hasher},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

#[derive(Clone)]
pub struct Server {
    port: u16,
    files: Arc<Mutex<HashMap<String, PathBuf>>>,
}

fn token() -> String {
    let mut out = String::new();
    for i in 0..2u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

impl Server {
    pub fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let files: Arc<Mutex<HashMap<String, PathBuf>>> = Arc::default();
        let f = files.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let f = f.clone();
                std::thread::spawn(move || {
                    let _ = handle(stream, &f);
                });
            }
        });
        Ok(Server { port, files })
    }

    /// Serve this file; returns its URL. Earlier files stop being served.
    pub fn share(&self, path: &Path) -> String {
        let t = token();
        let mut files = self.files.lock().unwrap();
        files.clear();
        files.insert(t.clone(), path.to_path_buf());
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        format!("http://127.0.0.1:{}/{t}/video.{ext}", self.port)
    }
}

fn mime(path: &Path) -> &'static str {
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov" | "qt") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("ogv") => "video/ogg",
        Some("avi") => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

/// "bytes=100-" / "bytes=100-199" / "bytes=-500" -> (first, last) inclusive.
pub fn parse_range(v: &str, len: u64) -> Option<(u64, u64)> {
    let r = v.trim().strip_prefix("bytes=")?;
    let r = r.split(',').next()?.trim();
    let (a, b) = r.split_once('-')?;
    if len == 0 {
        return None;
    }
    let (first, last) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (len.saturating_sub(n), len - 1)
    } else {
        let first: u64 = a.parse().ok()?;
        let last = if b.is_empty() {
            len - 1
        } else {
            b.parse::<u64>().ok()?.min(len - 1)
        };
        (first, last)
    };
    (first <= last && first < len).then_some((first, last))
}

fn handle(mut stream: TcpStream, files: &Mutex<HashMap<String, PathBuf>>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut range = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("range")
        {
            range = Some(v.trim().to_string());
        }
    }

    let reply = |s: &mut TcpStream, code: &str| {
        write!(s, "HTTP/1.1 {code}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    };
    if method != "GET" && method != "HEAD" {
        return reply(&mut stream, "405 Method Not Allowed");
    }
    let tok = target.trim_start_matches('/').split('/').next().unwrap_or("");
    let Some(path) = files.lock().unwrap().get(tok).cloned() else {
        return reply(&mut stream, "404 Not Found");
    };
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(_) => return reply(&mut stream, "404 Not Found"),
    };
    let len = file.metadata()?.len();

    let (status, first, last) = match range.as_deref() {
        Some(r) => match parse_range(r, len) {
            Some((a, b)) => ("206 Partial Content", a, b),
            None => {
                return write!(
                    stream,
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        },
        None => ("200 OK", 0, len.saturating_sub(1)),
    };
    let count = if len == 0 { 0 } else { last - first + 1 };
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {count}\r\nAccept-Ranges: bytes\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nConnection: close\r\n",
        mime(&path)
    );
    if status.starts_with("206") {
        head.push_str(&format!("Content-Range: bytes {first}-{last}/{len}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if method == "HEAD" || count == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(first))?;
    // the player often hangs up mid-transfer when it seeks: that's fine
    std::io::copy(&mut file.take(count), &mut stream)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-500", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-30", 100), Some((70, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=5-2", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
    }

    #[test]
    fn serves_only_shared_files_with_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.mp4");
        std::fs::write(&p, b"0123456789").unwrap();
        let s = Server::start().unwrap();
        let url = s.share(&p);
        let path = url.split_once(&format!(":{}", s.port)).unwrap().1.to_string();
        let get = |target: &str, extra: &str| {
            let mut c = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
            write!(c, "GET {target} HTTP/1.1\r\nHost: x\r\n{extra}\r\n").unwrap();
            let mut out = String::new();
            c.read_to_string(&mut out).unwrap();
            out
        };
        let r = get(&path, "Range: bytes=2-5\r\n");
        assert!(r.starts_with("HTTP/1.1 206"), "{r}");
        assert!(r.contains("Content-Range: bytes 2-5/10"));
        assert!(r.ends_with("\r\n\r\n2345"));
        let r = get(&path, "");
        assert!(r.starts_with("HTTP/1.1 200") && r.ends_with("0123456789"));
        assert!(get("/nope/video.mp4", "").starts_with("HTTP/1.1 404"));
        // sharing another file retires the old URL
        s.share(&p);
        assert!(get(&path, "").starts_with("HTTP/1.1 404"));
    }
}
