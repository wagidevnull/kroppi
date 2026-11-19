//! Everything kroppi asks of ffmpeg: reading a video's details (ffprobe),
//! building the trim/crop command, and running it with progress and cancel.

use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ------------------------------------------------------------------ probing

/// What kroppi needs to know about a video. Width and height are as it is
/// shown (rotation applied), which is also what crop coordinates refer to.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Probe {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub vcodec: String,
    pub acodec: Option<String>,
    pub rotation: i32,
    /// Has subtitle tracks. WebKitGTK stalls on these (it waits for subtitle
    /// data before it shows the first frame), so the window gets a remux
    /// without them instead.
    pub subtitles: bool,
    /// Container kroppi will write by default: mp4, mov, mkv or webm.
    pub container: String,
}

pub fn probe(path: &Path) -> Result<Probe, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_format", "-show_streams", "-of", "json"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "not a video ffmpeg can read: {}",
            tail(&String::from_utf8_lossy(&out.stderr))
        ));
    }
    let json: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    parse_probe(path, size, &json)
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// "30000/1001" -> 29.97
fn rate(v: &Value) -> Option<f64> {
    let s = v.as_str()?;
    let (a, b) = s.split_once('/').unwrap_or((s, "1"));
    let (a, b): (f64, f64) = (a.parse().ok()?, b.parse().ok()?);
    (a > 0.0 && b > 0.0).then(|| a / b)
}

pub fn parse_probe(path: &Path, size: u64, json: &Value) -> Result<Probe, String> {
    let streams = json["streams"].as_array().cloned().unwrap_or_default();
    // the first real video stream (not cover art)
    let v = streams
        .iter()
        .find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1)
        .ok_or("this file has no video")?;
    let a = streams.iter().find(|s| s["codec_type"] == "audio");

    let mut rotation = num(&v["tags"]["rotate"]).unwrap_or(0.0) as i32;
    if let Some(sd) = v["side_data_list"].as_array() {
        for d in sd {
            if let Some(r) = num(&d["rotation"]) {
                rotation = r as i32;
            }
        }
    }
    let rotation = rotation.rem_euclid(360);

    let (mut width, mut height) = (
        num(&v["width"]).unwrap_or(0.0) as u32,
        num(&v["height"]).unwrap_or(0.0) as u32,
    );
    if rotation == 90 || rotation == 270 {
        std::mem::swap(&mut width, &mut height);
    }
    if width == 0 || height == 0 {
        return Err("couldn't read the video size".into());
    }

    let duration = num(&json["format"]["duration"])
        .or_else(|| num(&v["duration"]))
        .filter(|d| d.is_finite() && *d > 0.0)
        .ok_or("couldn't read the video length")?;
    let fps = rate(&v["avg_frame_rate"])
        .or_else(|| rate(&v["r_frame_rate"]))
        .filter(|f| *f > 0.5 && *f < 1000.0)
        .unwrap_or(30.0);

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Probe {
        path: path.to_string_lossy().into_owned(),
        container: container_for(path, json["format"]["format_name"].as_str().unwrap_or("")).into(),
        name,
        size,
        duration,
        width,
        height,
        fps,
        vcodec: v["codec_name"].as_str().unwrap_or("?").into(),
        acodec: a.and_then(|a| a["codec_name"].as_str()).map(String::from),
        rotation,
        subtitles: streams.iter().any(|s| s["codec_type"] == "subtitle"),
    })
}

/// The container to write for this input: the same one where we can,
/// otherwise mp4.
pub fn container_for(path: &Path, format_name: &str) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" | "m4v" => "mp4",
        "mov" | "qt" => "mov",
        "mkv" => "mkv",
        "webm" => "webm",
        "" if format_name.starts_with("matroska") => "mkv",
        _ => "mp4",
    }
}

/// The container a path's extension asks for, if it's one kroppi writes.
pub fn container_from_ext(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_string_lossy().to_lowercase().as_str() {
        "mp4" | "m4v" => Some("mp4"),
        "mov" => Some("mov"),
        "mkv" => Some("mkv"),
        "webm" => Some("webm"),
        _ => None,
    }
}

// ------------------------------------------------------------------ export

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Re-encode: cuts land on the exact frame.
    Exact,
    /// Copy the streams: instant and lossless, cuts snap to keyframes.
    Fast,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    High,
    Medium,
    Small,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ExportOpts {
    pub input: PathBuf,
    pub output: PathBuf,
    pub start: f64,
    pub end: f64,
    /// In shown-video pixels. None = the whole frame.
    pub crop: Option<Crop>,
    pub mode: Mode,
    pub mute: bool,
    pub quality: Quality,
    /// Shown size of the input (from probe), to keep the crop inside it.
    pub width: u32,
    pub height: u32,
}

/// A crop ffmpeg will accept: whole even pixels, inside the frame. None when
/// it's the whole frame (and that frame is already even-sized).
pub fn even_crop(c: Option<Crop>, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let full = Crop {
        x: 0.0,
        y: 0.0,
        w: w as f64,
        h: h as f64,
    };
    let c = c.unwrap_or(full);
    let even = |v: f64| ((v.max(0.0) as u32) / 2) * 2;
    let cw = even(c.w.round().clamp(2.0, w as f64)).max(2);
    let ch = even(c.h.round().clamp(2.0, h as f64)).max(2);
    let cx = even(c.x.round().clamp(0.0, (w - cw) as f64));
    let cy = even(c.y.round().clamp(0.0, (h - ch) as f64));
    if (cx, cy, cw, ch) == (0, 0, w, h) {
        None
    } else {
        Some((cw, ch, cx, cy))
    }
}

/// Can the input's streams be copied into this container as they are?
pub fn fast_ok(input: &Path, out_container: &str) -> bool {
    out_container == "mkv" || container_for(input, "") == out_container && container_from_ext(input).is_some()
}

/// The export as it will actually run (after any fallbacks).
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub args: Vec<String>,
    pub container: &'static str,
    pub mode: Mode,
    /// Shown to the user, e.g. why fast mode wasn't used.
    pub note: Option<String>,
}

/// The ffmpeg arguments for an export, writing to `part`.
pub fn build_args(o: &ExportOpts, part: &Path) -> Result<Plan, String> {
    if !(o.end - o.start > 0.0) || o.start < 0.0 {
        return Err("the selection is empty".into());
    }
    let container = container_from_ext(&o.output).unwrap_or("mp4");
    let crop = even_crop(o.crop, o.width, o.height);
    let mut mode = o.mode;
    let mut note = None;
    if mode == Mode::Fast && o.crop.is_some() && crop.is_some() {
        mode = Mode::Exact;
        note = Some("cropping needs a re-encode, so exact mode was used".into());
    } else if mode == Mode::Fast && !fast_ok(&o.input, container) {
        mode = Mode::Exact;
        note = Some(format!("can't copy this video into {container}, so exact mode was used"));
    }

    let s = |v: &str| v.to_string();
    let mut a: Vec<String> = vec![s("-v"), s("error"), s("-nostdin"), s("-y")];
    a.extend([s("-ss"), format!("{:.3}", o.start)]);
    a.push(s("-i"));
    a.push(o.input.to_string_lossy().into_owned());
    a.extend([s("-t"), format!("{:.3}", o.end - o.start)]);
    a.extend([s("-map"), s("0:v:0")]);
    if o.mute {
        a.push(s("-an"));
    } else {
        a.extend([s("-map"), s("0:a:0?")]);
    }
    a.extend([s("-map_metadata"), s("0"), s("-map_chapters"), s("-1")]);

    match mode {
        Mode::Fast => {
            a.extend([s("-c"), s("copy"), s("-avoid_negative_ts"), s("make_zero")]);
        }
        Mode::Exact => {
            if let Some((w, h, x, y)) = crop {
                a.extend([s("-vf"), format!("crop={w}:{h}:{x}:{y}")]);
            }
            if container == "webm" {
                let crf = match o.quality {
                    Quality::High => "24",
                    Quality::Medium => "32",
                    Quality::Small => "40",
                };
                a.extend([s("-c:v"), s("libvpx-vp9"), s("-crf"), s(crf), s("-b:v"), s("0")]);
                a.extend([s("-row-mt"), s("1"), s("-deadline"), s("good"), s("-cpu-used"), s("2")]);
                a.extend([s("-pix_fmt"), s("yuv420p")]);
                if !o.mute {
                    a.extend([s("-c:a"), s("libopus"), s("-b:a"), s("160k")]);
                }
            } else {
                let crf = match o.quality {
                    Quality::High => "18",
                    Quality::Medium => "23",
                    Quality::Small => "28",
                };
                a.extend([s("-c:v"), s("libx264"), s("-preset"), s("medium"), s("-crf"), s(crf)]);
                a.extend([s("-pix_fmt"), s("yuv420p")]);
                if !o.mute {
                    a.extend([s("-c:a"), s("aac"), s("-b:a"), s("192k")]);
                }
            }
        }
    }
    if container == "mp4" || container == "mov" {
        a.extend([s("-movflags"), s("+faststart")]);
    }
    let muxer = match container {
        "mkv" => "matroska",
        c => c,
    };
    a.extend([s("-progress"), s("pipe:1"), s("-nostats"), s("-f"), s(muxer)]);
    a.push(part.to_string_lossy().into_owned());
    Ok(Plan {
        args: a,
        container,
        mode,
        note,
    })
}

/// The arguments for a copy of the video with only its first video and audio
/// track, streams copied as they are: as fast as the disk, and the window
/// plays it when the original's extra tracks (subtitles) stall it.
/// Matroska takes any codec, so the copy never fails on the container.
pub fn remux_args(input: &Path, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["-v", "error", "-nostdin", "-y", "-i"]
        .map(String::from)
        .to_vec();
    a.push(input.to_string_lossy().into_owned());
    a.extend(
        [
            "-map", "0:v:0", "-map", "0:a:0?", "-c", "copy", "-sn", "-dn", "-progress", "pipe:1",
            "-nostats", "-f", "matroska",
        ]
        .map(String::from),
    );
    a.push(out.to_string_lossy().into_owned());
    a
}

/// The arguments for a small preview copy of a video the window can't play.
pub fn proxy_args(input: &Path, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["-v", "error", "-nostdin", "-y", "-i"]
        .map(String::from)
        .to_vec();
    a.push(input.to_string_lossy().into_owned());
    a.extend(
        [
            "-map", "0:v:0", "-map", "0:a:0?", "-vf", "scale=-2:'min(720,ih)'", "-c:v", "libx264",
            "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a",
            "128k", "-movflags", "+faststart", "-progress", "pipe:1", "-nostats",
        ]
        .map(String::from),
    );
    a.push(out.to_string_lossy().into_owned());
    a
}

// ------------------------------------------------------------------ running

/// Seconds done, from one line of `-progress` output.
pub fn progress_secs(line: &str) -> Option<f64> {
    let v = line.strip_prefix("out_time_us=")?.trim();
    let us: f64 = v.parse().ok()?;
    (us >= 0.0).then(|| us / 1e6)
}

/// The last bit of ffmpeg's error output, for showing to the user.
pub fn tail(err: &str) -> String {
    let err = err.trim();
    let start = err
        .char_indices()
        .rev()
        .nth(400)
        .map(|(i, _)| i)
        .unwrap_or(0);
    err[start..].to_string()
}

/// Run ffmpeg, reporting progress (0..1) against `dur` seconds of output.
/// The child is kept in `slot` while it runs, so it can be killed to cancel.
pub fn run(
    args: &[String],
    dur: f64,
    slot: &Mutex<Option<Child>>,
    mut on_progress: impl FnMut(f64),
) -> Result<(), String> {
    let mut child = Command::new("ffmpeg")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run ffmpeg: {e}"))?;
    let stdout = child.stdout.take().ok_or("no ffmpeg output")?;
    let mut stderr = child.stderr.take().ok_or("no ffmpeg output")?;
    *slot.lock().unwrap() = Some(child);
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if let Some(t) = progress_secs(&line)
            && dur > 0.0
        {
            on_progress((t / dur).clamp(0.0, 1.0));
        }
    }
    let err = err_reader.join().unwrap_or_default();
    // export_cancel kills the child but leaves it here; None means it's gone.
    let child = slot.lock().unwrap().take();
    let status = match child {
        Some(mut c) => c.wait().map_err(|e| e.to_string())?,
        None => return Err("cancelled".into()),
    };
    if !status.success() {
        return Err(format!("ffmpeg failed: {}", tail(&err)));
    }
    on_progress(1.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opts() -> ExportOpts {
        ExportOpts {
            input: "/v/in.mp4".into(),
            output: "/v/out.mp4".into(),
            start: 2.0,
            end: 5.5,
            crop: None,
            mode: Mode::Exact,
            mute: false,
            quality: Quality::Medium,
            width: 1920,
            height: 1080,
        }
    }
    fn has(args: &[String], pair: &[&str]) -> bool {
        args.windows(pair.len()).any(|w| w.iter().zip(pair).all(|(a, b)| a == b))
    }

    #[test]
    fn crops_are_even_and_inside_the_frame() {
        let c = |x, y, w, h| Some(Crop { x, y, w, h });
        assert_eq!(even_crop(c(240.0, 0.0, 1440.0, 1080.0), 1920, 1080), Some((1440, 1080, 240, 0)));
        assert_eq!(even_crop(c(3.4, 5.0, 101.0, 77.0), 1920, 1080), Some((100, 76, 2, 4)));
        // too big or off the edge gets pulled back in
        assert_eq!(even_crop(c(1800.0, 900.0, 400.0, 400.0), 1920, 1080), Some((400, 400, 1520, 680)));
        // the whole frame is no crop at all
        assert_eq!(even_crop(c(0.0, 0.0, 1920.0, 1080.0), 1920, 1080), None);
        assert_eq!(even_crop(None, 1920, 1080), None);
        // odd-sized video: trim to even even with no crop asked for
        assert_eq!(even_crop(None, 641, 481), Some((640, 480, 0, 0)));
    }

    #[test]
    fn exact_trim_with_crop() {
        let mut o = opts();
        o.crop = Some(Crop { x: 240.0, y: 0.0, w: 1440.0, h: 1080.0 });
        let p = build_args(&o, Path::new("/v/.out.part")).unwrap();
        assert_eq!(p.mode, Mode::Exact);
        assert!(has(&p.args, &["-ss", "2.000", "-i", "/v/in.mp4", "-t", "3.500"]));
        assert!(has(&p.args, &["-vf", "crop=1440:1080:240:0"]));
        assert!(has(&p.args, &["-c:v", "libx264"]));
        assert!(has(&p.args, &["-crf", "23"]));
        assert!(has(&p.args, &["-map", "0:a:0?"]));
        assert!(has(&p.args, &["-c:a", "aac"]));
        assert!(has(&p.args, &["-f", "mp4", "/v/.out.part"]));
        assert!(has(&p.args, &["-movflags", "+faststart"]));
    }

    #[test]
    fn fast_trim_copies() {
        let mut o = opts();
        o.mode = Mode::Fast;
        let p = build_args(&o, Path::new("/v/p")).unwrap();
        assert_eq!(p.mode, Mode::Fast);
        assert!(has(&p.args, &["-c", "copy"]));
        assert!(!p.args.iter().any(|a| a == "-vf"));
        assert_eq!(p.note, None);
    }

    #[test]
    fn fast_with_crop_falls_back_to_exact() {
        let mut o = opts();
        o.mode = Mode::Fast;
        o.crop = Some(Crop { x: 0.0, y: 0.0, w: 1000.0, h: 1000.0 });
        let p = build_args(&o, Path::new("/v/p")).unwrap();
        assert_eq!(p.mode, Mode::Exact);
        assert!(p.note.is_some());
        assert!(!has(&p.args, &["-c", "copy"]));
    }

    #[test]
    fn fast_into_a_foreign_container_falls_back() {
        let mut o = opts();
        o.mode = Mode::Fast;
        o.input = "/v/in.avi".into();
        let p = build_args(&o, Path::new("/v/p")).unwrap();
        assert_eq!(p.mode, Mode::Exact);
        // ... but mkv holds anything
        o.output = "/v/out.mkv".into();
        let p = build_args(&o, Path::new("/v/p")).unwrap();
        assert_eq!(p.mode, Mode::Fast);
        assert!(has(&p.args, &["-f", "matroska"]));
    }

    #[test]
    fn webm_uses_vp9_and_opus() {
        let mut o = opts();
        o.output = "/v/out.webm".into();
        o.quality = Quality::High;
        let p = build_args(&o, Path::new("/v/p")).unwrap();
        assert_eq!(p.container, "webm");
        assert!(has(&p.args, &["-c:v", "libvpx-vp9", "-crf", "24"]));
        assert!(has(&p.args, &["-c:a", "libopus"]));
        assert!(!p.args.iter().any(|a| a == "-movflags"));
    }

    #[test]
    fn mute_drops_audio() {
        let mut o = opts();
        o.mute = true;
        let p = build_args(&o, Path::new("/v/p")).unwrap();
        assert!(p.args.iter().any(|a| a == "-an"));
        assert!(!has(&p.args, &["-map", "0:a:0?"]));
        assert!(!p.args.iter().any(|a| a == "-c:a"));
    }

    #[test]
    fn empty_selection_is_refused() {
        let mut o = opts();
        o.end = o.start;
        assert!(build_args(&o, Path::new("/v/p")).is_err());
    }

    #[test]
    fn containers() {
        assert_eq!(container_for(Path::new("a.MP4"), ""), "mp4");
        assert_eq!(container_for(Path::new("a.mov"), ""), "mov");
        assert_eq!(container_for(Path::new("a.mkv"), ""), "mkv");
        assert_eq!(container_for(Path::new("a.webm"), ""), "webm");
        assert_eq!(container_for(Path::new("a.avi"), "avi"), "mp4");
        assert_eq!(container_for(Path::new("noext"), "matroska,webm"), "mkv");
        assert_eq!(container_from_ext(Path::new("x.mkv")), Some("mkv"));
        assert_eq!(container_from_ext(Path::new("x.avi")), None);
    }

    #[test]
    fn progress_lines() {
        assert_eq!(progress_secs("out_time_us=1500000"), Some(1.5));
        assert_eq!(progress_secs("out_time_us=N/A"), None);
        assert_eq!(progress_secs("out_time_us=-5"), None);
        assert_eq!(progress_secs("frame=12"), None);
    }

    #[test]
    fn probe_json_with_rotation() {
        let j = json!({
            "streams": [
                {"codec_type": "video", "codec_name": "mjpeg", "width": 300, "height": 300, "disposition": {"attached_pic": 1}},
                {"codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080,
                 "avg_frame_rate": "30000/1001", "disposition": {"attached_pic": 0},
                 "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]},
                {"codec_type": "audio", "codec_name": "aac"}
            ],
            "format": {"duration": "12.5", "format_name": "mov,mp4,m4a,3gp,3g2,mj2"}
        });
        let p = parse_probe(Path::new("/x/phone.mp4"), 99, &j).unwrap();
        assert_eq!((p.width, p.height), (1080, 1920));
        assert_eq!(p.rotation, 270);
        assert!((p.fps - 29.97).abs() < 0.01);
        assert_eq!(p.duration, 12.5);
        assert_eq!(p.vcodec, "h264");
        assert_eq!(p.acodec.as_deref(), Some("aac"));
        assert_eq!(p.container, "mp4");
        assert_eq!(p.name, "phone.mp4");
        assert!(!p.subtitles);
    }

    #[test]
    fn probe_json_with_subtitles() {
        let j = json!({
            "streams": [
                {"codec_type": "video", "codec_name": "h264", "width": 1920, "height": 960},
                {"codec_type": "audio", "codec_name": "eac3"},
                {"codec_type": "subtitle", "codec_name": "subrip"}
            ],
            "format": {"duration": "60", "format_name": "matroska,webm"}
        });
        assert!(parse_probe(Path::new("/x/ep.mkv"), 1, &j).unwrap().subtitles);
    }

    #[test]
    fn remux_keeps_one_video_and_audio_track_copied() {
        let a = remux_args(Path::new("/v/in.mkv"), Path::new("/c/preview.mkv"));
        assert!(has(&a, &["-map", "0:v:0", "-map", "0:a:0?", "-c", "copy", "-sn"]));
        assert!(has(&a, &["-f", "matroska", "/c/preview.mkv"]));
    }

    #[test]
    fn probe_json_without_video() {
        let j = json!({"streams": [{"codec_type": "audio"}], "format": {"duration": "3"}});
        assert!(parse_probe(Path::new("a.mp3"), 0, &j).is_err());
    }

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
            && Command::new("ffprobe").arg("-version").output().is_ok()
    }

    /// A real export, end to end: trim 1.0-2.5s of a 1280x720 clip, crop to 4:3.
    #[test]
    fn real_export() {
        if !have_ffmpeg() {
            eprintln!("skipping: no ffmpeg");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mp4");
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=1280x720:rate=25:duration=4"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=4"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&src)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let pr = probe(&src).unwrap();
        assert_eq!((pr.width, pr.height), (1280, 720));
        assert_eq!(pr.fps, 25.0);
        assert_eq!(pr.acodec.as_deref(), Some("aac"));

        let out = dir.path().join("out.mp4");
        let part = dir.path().join(".out.part");
        let mut o = opts();
        o.input = src.clone();
        o.output = out.clone();
        o.start = 1.0;
        o.end = 2.5;
        o.width = 1280;
        o.height = 720;
        o.crop = Some(Crop { x: 160.0, y: 0.0, w: 960.0, h: 720.0 });
        let plan = build_args(&o, &part).unwrap();
        let slot = Mutex::new(None);
        let mut last = 0.0;
        run(&plan.args, 1.5, &slot, |p| last = p).unwrap();
        assert_eq!(last, 1.0);
        std::fs::rename(&part, &out).unwrap();
        let r = probe(&out).unwrap();
        assert_eq!((r.width, r.height), (960, 720));
        assert!((r.duration - 1.5).abs() < 0.1, "duration {}", r.duration);
        assert!(r.acodec.is_some());
    }
}
