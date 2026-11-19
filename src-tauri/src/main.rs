//! kroppi: trim and crop videos.
//!
//! The interface (ui/) runs in a native window and streams the video from a
//! small local server (see serve.rs). Rust does the rest: reading the video's details with ffprobe,
//! the open/save dialogs, running ffmpeg for the export, and asking before
//! quitting with unsaved work.

mod ffmpeg;
mod serve;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent, ipc::Channel};
use tauri_plugin_dialog::DialogExt;

const USAGE: &str = "\
kroppi: trim and crop videos

usage: kroppi [VIDEO]

  VIDEO        open this video straight away
  --version    print the version
  -h, --help   show this help

kroppi needs ffmpeg and ffprobe.";

const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "ts", "m2ts", "mts",
    "3gp", "ogv", "gif",
];

#[derive(Default)]
struct AppState {
    initial: Option<PathBuf>,
    server: Option<serve::Server>,
    /// A video is loaded and nothing has been exported since.
    dirty: AtomicBool,
    quitting: AtomicBool,
    /// ffmpeg is running (an export or a preview copy).
    busy: AtomicBool,
    child: Mutex<Option<Child>>,
    /// Holds the preview copy of a video the window couldn't play.
    proxy: Mutex<Option<tempfile::TempDir>>,
}

type Res<T> = Result<T, String>;

fn have(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(cmd).is_file()))
}

/// Marks ffmpeg as running until dropped.
struct Busy<'a>(&'a AtomicBool);
impl<'a> Busy<'a> {
    fn take(flag: &'a AtomicBool) -> Res<Self> {
        if flag.swap(true, Ordering::SeqCst) {
            return Err("ffmpeg is already busy".into());
        }
        Ok(Busy(flag))
    }
}
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------------ app info

#[derive(Serialize)]
struct Info {
    version: &'static str,
    ffmpeg: bool,
}

#[tauri::command]
fn app_info() -> Info {
    Info {
        version: env!("CARGO_PKG_VERSION"),
        ffmpeg: have("ffmpeg") && have("ffprobe"),
    }
}

/// The video given on the command line ("Open with kroppi"), if any.
#[tauri::command]
fn initial_video(state: State<AppState>) -> Option<String> {
    state
        .initial
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
}

// ------------------------------------------------------------------ opening

#[tauri::command]
async fn pick_video(app: AppHandle) -> Res<Option<String>> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut d = app
            .dialog()
            .file()
            .set_title("Open a video")
            .add_filter("Videos", VIDEO_EXTS)
            .add_filter("All files", &["*"]);
        if let Some(w) = app.get_webview_window("main") {
            d = d.set_parent(&w);
        }
        d.blocking_pick_file()
            .and_then(|f| f.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct Opened {
    #[serde(flatten)]
    probe: ffmpeg::Probe,
    /// Where the window can stream it from.
    url: String,
}

fn share(state: &AppState, path: &Path) -> Res<String> {
    let server = state.server.as_ref().ok_or("the video server didn't start")?;
    Ok(server.share(path))
}

/// Read a video's details and let the window stream it.
#[tauri::command]
async fn open_video(state: State<'_, AppState>, path: String) -> Res<Opened> {
    let path = PathBuf::from(path);
    if !path.is_file() {
        return Err(format!("no such file: {}", path.display()));
    }
    let p = path.clone();
    let probe = tauri::async_runtime::spawn_blocking(move || ffmpeg::probe(&p))
        .await
        .map_err(|e| e.to_string())??;
    Ok(Opened {
        url: share(&state, &path)?,
        probe,
    })
}

/// Where preview copies go. Not /tmp: that is often RAM (tmpfs), and a remux
/// is as big as the original.
fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("kroppi")
}

/// Make a copy of a video the window can't play, for the preview. `kind` is
/// "remux" (copy the first video and audio track, dropping subtitles and the
/// rest: fast) or "encode" (a small mp4: slow, but plays anything ffmpeg
/// reads). Exports always read the original.
#[tauri::command]
async fn make_proxy(
    app: AppHandle,
    path: String,
    duration: f64,
    kind: String,
    on_progress: Channel<f64>,
) -> Res<String> {
    let a = app.clone();
    let out = tauri::async_runtime::spawn_blocking(move || -> Res<PathBuf> {
        let state = a.state::<AppState>();
        let _busy = Busy::take(&state.busy)?;
        // the previous copy is deleted (its TempDir dropped) before making this one
        state.proxy.lock().unwrap().take();
        let base = cache_dir();
        fs::create_dir_all(&base).map_err(|e| format!("couldn't make {}: {e}", base.display()))?;
        // the pid in the name tells sweep_stale_previews whose copy it is
        let dir = tempfile::Builder::new()
            .prefix(&format!("preview-{}-", std::process::id()))
            .tempdir_in(&base)
            .map_err(|e| e.to_string())?;
        let (out, args) = match kind.as_str() {
            "remux" => {
                let out = dir.path().join("preview.mkv");
                let args = ffmpeg::remux_args(Path::new(&path), &out);
                (out, args)
            }
            _ => {
                let out = dir.path().join("preview.mp4");
                let args = ffmpeg::proxy_args(Path::new(&path), &out);
                (out, args)
            }
        };
        ffmpeg::run(&args, duration, &state.child, throttled(&on_progress))?;
        *state.proxy.lock().unwrap() = Some(dir);
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())??;
    share(&app.state::<AppState>(), &out)
}

/// Progress updates, at most ~100 per run.
fn throttled(ch: &Channel<f64>) -> impl FnMut(f64) + '_ {
    let mut last = -1.0;
    move |p| {
        if p >= 1.0 || p - last >= 0.01 {
            last = p;
            let _ = ch.send(p);
        }
    }
}

// ------------------------------------------------------------------ exporting

/// Ask where to save. `suggested` is the full suggested path.
#[tauri::command]
async fn pick_save(app: AppHandle, suggested: String) -> Res<Option<String>> {
    #[cfg(feature = "autotest")]
    if let Ok(p) = std::env::var("KROPPI_AUTOTEST_OUT") {
        let _ = &suggested;
        return Ok(Some(p));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let s = PathBuf::from(&suggested);
        let ext = s
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mp4".into());
        let mut d = app
            .dialog()
            .file()
            .set_title("Save video as")
            .add_filter(ext.to_uppercase(), &[ext.as_str()])
            .add_filter("Other formats (mp4, mov, mkv, webm)", &["mp4", "mov", "mkv", "webm"]);
        if let Some(dir) = s.parent().filter(|d| d.is_dir()) {
            d = d.set_directory(dir);
        }
        if let Some(name) = s.file_name() {
            d = d.set_file_name(name.to_string_lossy());
        }
        if let Some(w) = app.get_webview_window("main") {
            d = d.set_parent(&w);
        }
        d.blocking_save_file()
            .and_then(|f| f.into_path().ok())
            .map(|p| {
                // no known extension: use the suggested one
                if ffmpeg::container_from_ext(&p).is_some() {
                    p
                } else {
                    let mut name = p.file_name().unwrap_or_default().to_os_string();
                    name.push(format!(".{ext}"));
                    p.with_file_name(name)
                }
            })
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())
}

/// Are these the same file on disk? Compares device and inode, so another
/// path to the same file (a symlink, "..", a hard link) counts too. A path
/// that doesn't exist yet is never the same file.
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Would saving to `output` overwrite the video being read?
#[tauri::command]
fn is_same_file(input: String, output: String) -> bool {
    same_file(Path::new(&input), Path::new(&output))
}

#[derive(Serialize)]
struct Exported {
    path: String,
    size: u64,
    mode: &'static str,
    note: Option<String>,
}

#[tauri::command]
async fn export(app: AppHandle, opts: ffmpeg::ExportOpts, on_progress: Channel<f64>) -> Res<Exported> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _busy = Busy::take(&state.busy)?;
        let out = opts.output.clone();
        // the window asks first (is_same_file); this is the backstop
        if same_file(&opts.input, &out) {
            return Err("that's the video kroppi is reading: pick a different file".into());
        }
        let dir = out.parent().ok_or("bad output path")?;
        let name = out.file_name().ok_or("bad output path")?.to_string_lossy();
        // write next to the target, then move into place: no half-written files
        let part = dir.join(format!(".{name}.kroppi-part"));
        let plan = ffmpeg::build_args(&opts, &part)?;
        let r = ffmpeg::run(&plan.args, opts.end - opts.start, &state.child, throttled(&on_progress));
        if let Err(e) = r {
            let _ = fs::remove_file(&part);
            return Err(e);
        }
        fs::rename(&part, &out).map_err(|e| {
            let _ = fs::remove_file(&part);
            format!("couldn't save {}: {e}", out.display())
        })?;
        Ok(Exported {
            size: fs::metadata(&out).map(|m| m.len()).unwrap_or(0),
            path: out.to_string_lossy().into_owned(),
            mode: match plan.mode {
                ffmpeg::Mode::Exact => "exact",
                ffmpeg::Mode::Fast => "fast",
            },
            note: plan.note,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stop the running export or preview copy.
#[tauri::command]
fn cancel(state: State<AppState>) {
    // take it out of the slot so run() knows it was cancelled
    if let Some(mut c) = state.child.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Open the folder a file is in.
#[tauri::command]
fn reveal(path: String) {
    let p = Path::new(&path);
    let dir = if p.is_dir() {
        p
    } else {
        p.parent().unwrap_or(p)
    };
    if dir.is_dir() {
        let _ = Command::new("xdg-open")
            .arg(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

// ------------------------------------------------------------------ quitting

#[tauri::command]
fn set_dirty(state: State<AppState>, dirty: bool) {
    state.dirty.store(dirty, Ordering::Relaxed);
}

#[tauri::command]
fn quit(app: AppHandle, state: State<AppState>) {
    state.quitting.store(true, Ordering::Relaxed);
    cancel(state);
    app.exit(0);
}

// ------------------------------------------------------------------ test hooks

#[cfg(feature = "autotest")]
#[tauri::command]
fn autotest_report(text: String) {
    println!("{text}");
}

#[cfg(feature = "autotest")]
#[tauri::command]
fn autotest_exit(code: i32) {
    std::process::exit(code);
}

// ------------------------------------------------------------------ main

/// GStreamer's VA-API decoders (gstreamer1.0-plugins-bad) hand WebKitGTK GPU
/// frames it can't read on some drivers (Intel HD 630 on Debian 13: "DMABuf
/// caps negotiated without the mandatory support of VideoMeta"), so the
/// preview shows noise or nothing. Ranking them out makes GStreamer use the
/// software decoders, which are plenty for a preview. A rank the user set in
/// GST_PLUGIN_FEATURE_RANK comes later in the list, so it still wins.
const DEMOTED_DECODERS: &[&str] = &[
    "vah264dec", "vah265dec", "vavp8dec", "vavp9dec", "vaav1dec", "vampeg2dec", "vajpegdec",
    "vaapidecodebin", "vaapih264dec", "vaapih265dec", "vaapivp8dec", "vaapivp9dec", "vaapiav1dec",
    "vaapimpeg2dec",
];

fn demote_hw_decoders() {
    if std::env::var_os("KROPPI_HW_DECODE").is_some() {
        return;
    }
    let mut ranks: Vec<String> = DEMOTED_DECODERS.iter().map(|d| format!("{d}:NONE")).collect();
    if let Ok(user) = std::env::var("GST_PLUGIN_FEATURE_RANK")
        && !user.trim().is_empty()
    {
        ranks.push(user);
    }
    // SAFETY: called first thing in main, before any other thread exists.
    unsafe { std::env::set_var("GST_PLUGIN_FEATURE_RANK", ranks.join(",")) };
}

/// Delete preview copies left by a kroppi that is no longer running (killed
/// with -9, a crash, power loss). Folders are named preview-<pid>-<random>;
/// a copy whose kroppi is still running is kept.
fn sweep_stale_previews() {
    let Ok(entries) = fs::read_dir(cache_dir()) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("preview-") else { continue };
        let alive = rest
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok())
            .is_some_and(|pid| {
                fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|comm| comm.trim() == "kroppi")
            });
        if !alive {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

/// On SIGTERM/SIGINT/SIGHUP (logout, shutdown, Ctrl+C), stop ffmpeg and
/// delete the preview copy before exiting: Tauri's exit event doesn't run.
fn clean_up_on_signals(app: AppHandle) {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = match signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kroppi: couldn't watch for signals: {e}");
            return;
        }
    };
    std::thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            let state = app.state::<AppState>();
            if let Some(mut c) = state.child.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            state.proxy.lock().unwrap().take();
            std::process::exit(128 + sig);
        }
    });
}

fn main() {
    demote_hw_decoders();
    let mut initial = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("kroppi {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            a if a.starts_with('-') => {
                eprintln!("kroppi: unknown option {a}\n\n{USAGE}");
                std::process::exit(2);
            }
            a => {
                let mut p = PathBuf::from(a.strip_prefix("file://").unwrap_or(a));
                // An AppImage starts in its own folder; OWD is where it was run from.
                if p.is_relative()
                    && let Some(owd) = std::env::var_os("OWD")
                {
                    p = PathBuf::from(owd).join(p);
                }
                match p.canonicalize() {
                    Ok(p) if p.is_file() => initial = Some(p),
                    _ => {
                        eprintln!("kroppi: no such file: {a}");
                        std::process::exit(2);
                    }
                }
            }
        }
    }

    std::thread::spawn(sweep_stale_previews);

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            clean_up_on_signals(app.handle().clone());
            Ok(())
        })
        .manage(AppState {
            initial,
            server: serve::Server::start()
                .inspect_err(|e| eprintln!("kroppi: couldn't start the video server: {e}"))
                .ok(),
            ..Default::default()
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                if state.dirty.load(Ordering::Relaxed) && !state.quitting.load(Ordering::Relaxed) {
                    api.prevent_close();
                    let _ = window.emit("ask-quit", ());
                } else if let Some(mut c) = state.child.lock().unwrap().take() {
                    let _ = c.kill();
                }
            }
        });

    #[cfg(feature = "autotest")]
    let builder = builder
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Finished {
                if let Ok(path) = std::env::var("KROPPI_AUTOTEST") {
                    let js = fs::read_to_string(&path).expect("autotest script");
                    webview.eval(&js).expect("eval autotest script");
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            initial_video,
            pick_video,
            open_video,
            make_proxy,
            pick_save,
            is_same_file,
            export,
            cancel,
            reveal,
            set_dirty,
            quit,
            autotest_report,
            autotest_exit
        ]);

    #[cfg(not(feature = "autotest"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        app_info,
        initial_video,
        pick_video,
        open_video,
        make_proxy,
        pick_save,
        is_same_file,
        export,
        cancel,
        reveal,
        set_dirty,
        quit
    ]);

    let app = match builder.build(tauri::generate_context!()) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("kroppi: couldn't start: {e}");
            std::process::exit(1);
        }
    };
    app.run(|app, event| {
        // exit() doesn't drop the app state: delete the preview copy here
        if let tauri::RunEvent::Exit = event {
            app.state::<AppState>().proxy.lock().unwrap().take();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_file_sees_through_other_paths() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        fs::write(&a, "x").unwrap();
        fs::write(&b, "x").unwrap();
        let link = dir.path().join("link.mp4");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let dotted = dir.path().join("sub/../a.mp4");
        fs::create_dir(dir.path().join("sub")).unwrap();

        assert!(same_file(&a, &a));
        assert!(same_file(&a, &link), "a symlink to the open video");
        assert!(same_file(&a, &dotted), "a path with ..");
        assert!(!same_file(&a, &b), "a different file with the same contents");
        assert!(!same_file(&a, &dir.path().join("new.mp4")), "a file that doesn't exist yet");
    }
}
