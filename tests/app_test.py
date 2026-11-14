#!/usr/bin/env python3
"""Drive the real kroppi window and check what it does.

  python3 tests/app_test.py

Builds kroppi with the `autotest` feature (a hook that runs a test script in
the window, and skips the save dialog), makes some test videos with ffmpeg,
then:

  crop     open a video, trim 2.0-5.5 s, crop to 4:3, export; then cancel an export
  fast     trim without a crop in fast mode: the streams must be copied
  proxy    open an AVI the window can't play (preview copy), export it to mp4
  quit     close the window with unsaved work: it must ask first

Needs ffmpeg, a desktop session, and (for the quit test) Hyprland's hyprctl.
"""

import glob
import json
import os
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "src-tauri", "target", "release", "kroppi")
failures = 0

COMMON = r"""
const errors = [];
window.addEventListener('error', (e) => errors.push(String(e.message)));
window.addEventListener('unhandledrejection', (e) => errors.push(String(e.reason)));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (f, ms = 20000) => { const t = Date.now(); while (!f()) { if (Date.now() - t > ms) throw new Error('timeout'); await sleep(50); } };
const inv = (c, a) => window.__TAURI__.core.invoke(c, a);
const report = (o) => inv('autotest_report', { text: 'RESULT ' + JSON.stringify(o) });
const done = async (o) => { o.errors = errors; await report(o); await inv('autotest_exit', { code: 0 }); };
const fail = (e) => done({ fatal: String(e && e.stack || e) });
const $ = (s) => document.querySelector(s);
"""

SCRIPTS = {
    "crop": r"""
(async () => {
  const k = window.kroppi;
  await until(() => k.S.loaded);
  const r = { name: k.S.probe.name, dur: k.S.dur, vw: k.S.vw, vh: k.S.vh, playable: k.S.loaded };
  k.setStart(2); k.setEnd(5.5);
  k.setCropAspect('1.3333');
  r.crop = Object.assign({}, k.S.crop);
  r.fastLocked = $('#mode-seg [data-mode=fast]').classList.contains('locked');
  r.summary = $('#sum-size').textContent;
  k.setView('result');
  r.canvasRatio = $('#preview').width / $('#preview').height;
  k.setView('edit');
  await k.exportVideo();
  r.meta = $('#exp-meta').textContent;
  r.path = $('#exp-path').textContent;
  r.savedAfter = k.S.saved;
  $('[data-action=export-close]').click();
  // cancel part-way through
  k.setStart(0); k.setEnd(k.S.dur);
  const p = k.exportVideo();
  await sleep(250);
  $('[data-action=export-cancel]').click();
  await p;
  r.cancelStatus = $('#status').textContent;
  await done(r);
})().catch(fail);
""",
    "fast": r"""
(async () => {
  const k = window.kroppi;
  await until(() => k.S.loaded);
  k.setMode('fast');
  k.setStart(2); k.setEnd(4);
  const r = { hint: $('#mode-hint').textContent, fmt: $('#sum-fmt').textContent };
  await k.exportVideo();
  r.meta = $('#exp-meta').textContent;
  // a crop forces exact mode
  k.setCropAspect('1');
  r.modeWhileCropped = $('#mode-seg button.on').dataset.mode;
  // the window shares saved settings with the real app: put the mode back
  k.setCropAspect('free'); window.kroppi.S.crop = { x: 0, y: 0, w: k.S.vw, h: k.S.vh }; k.setMode('exact');
  await done(r);
})().catch(fail);
""",
    "proxy": r"""
(async () => {
  const k = window.kroppi;
  await until(() => k.S.loaded, 60000);
  const r = { name: k.S.probe.name, vw: k.S.vw, vh: k.S.vh, container: k.S.probe.container };
  k.setStart(0.5); k.setEnd(2);
  k.setCropAspect('1');
  await k.exportVideo();
  r.meta = $('#exp-meta').textContent;
  await done(r);
})().catch(fail);
""",
    "quit": r"""
(async () => {
  const k = window.kroppi;
  await until(() => k.S.loaded);
  await sleep(500);
  await report({ ready: true });
  // the test closes the window now; the quit prompt must appear instead
  await until(() => !$('#modal-quit').classList.contains('hidden'), 15000);
  await report({ prompt: true, errors });
  await sleep(4000); // the test checks the app is still running meanwhile
  $('[data-action=quit-yes]').click();
})().catch(fail);
""",
}


def check(ok, what):
    global failures
    print(("  ok    " if ok else "  FAIL  ") + what, flush=True)
    failures += not ok


def make_videos(tmp):
    def ff(*args):
        subprocess.run(["ffmpeg", "-v", "error", "-y", *args], check=True)
    mp4 = os.path.join(tmp, "test.mp4")
    ff("-f", "lavfi", "-i", "testsrc2=s=1280x720:r=30:d=8", "-f", "lavfi", "-i", "sine=f=440:d=8",
       "-c:v", "libx264", "-g", "30", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest", mp4)
    avi = os.path.join(tmp, "test.avi")
    ff("-f", "lavfi", "-i", "testsrc=s=320x240:r=25:d=3", "-c:v", "mpeg4", avi)
    return mp4, avi


def run(mode, video, tmp, out, during=None):
    script = os.path.join(tmp, f"{mode}.js")
    with open(script, "w") as f:
        f.write(COMMON + SCRIPTS[mode])
    env = dict(os.environ, KROPPI_AUTOTEST=script, KROPPI_AUTOTEST_OUT=out)
    proc = subprocess.Popen([BIN, video], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    results = []
    try:
        for line in proc.stdout:
            if line.startswith("RESULT "):
                results.append(json.loads(line[7:]))
                if during:
                    during(proc, results[-1])
        proc.wait(60)
    except subprocess.TimeoutExpired:
        proc.kill()
    if any("fatal" in r for r in results):
        print(proc.stderr.read()[-2000:])
    return proc, results


def probe(path):
    out = subprocess.run(["ffprobe", "-v", "error", "-show_format", "-show_streams", "-of", "json", path],
                         capture_output=True, text=True).stdout
    j = json.loads(out or "{}")
    v = next((s for s in j.get("streams", []) if s["codec_type"] == "video"), {})
    a = next((s for s in j.get("streams", []) if s["codec_type"] == "audio"), None)
    return {"w": v.get("width"), "h": v.get("height"), "vcodec": v.get("codec_name"),
            "audio": a and a.get("codec_name"), "dur": float(j.get("format", {}).get("duration", 0)),
            "format": j.get("format", {}).get("format_name")}


def close_window(pid):
    subprocess.run(["hyprctl", "dispatch", f'hl.dsp.focus({{ window = "pid:{pid}" }})'], capture_output=True)
    time.sleep(0.5)
    subprocess.run(["hyprctl", "dispatch", "hl.dsp.window.close()"], capture_output=True)


def result(res):
    r = res[-1] if res else {}
    check(not r.get("fatal"), "script ran" + (f": {r.get('fatal')}" if r.get("fatal") else ""))
    return r


def main():
    print("building (cargo build --release --features autotest)", flush=True)
    subprocess.run(["cargo", "build", "--release", "--quiet", "--features", "autotest"],
                   cwd=os.path.join(ROOT, "src-tauri"), check=True)
    with tempfile.TemporaryDirectory(prefix="kroppi-test-") as tmp:
        mp4, avi = make_videos(tmp)
        os.mkdir(os.path.join(tmp, "out"))

        print("crop")
        out = os.path.join(tmp, "out", "cropped.mp4")
        _, res = run("crop", mp4, tmp, out)
        r = result(res)
        check(r.get("name") == "test.mp4" and abs(r.get("dur", 0) - 8) < 0.1, f"video from the command line opened ({r.get('name')}, {r.get('dur')} s)")
        check(r.get("crop") == {"x": 160, "y": 0, "w": 960, "h": 720}, f"4:3 preset gives the biggest centred crop ({r.get('crop')})")
        check(r.get("fastLocked") is True, "fast mode is locked while cropped")
        check(abs(r.get("canvasRatio", 0) - 4 / 3) < 0.02, f"result view shows the cropped shape ({r.get('canvasRatio')})")
        check(r.get("path") == out and r.get("savedAfter") is True, f"export finished: {r.get('meta')}")
        if os.path.isfile(out):
            p = probe(out)
            check(p["w"] == 960 and p["h"] == 720, f"output is cropped to 960x720 ({p['w']}x{p['h']})")
            check(abs(p["dur"] - 3.5) < 0.1, f"output is trimmed to 3.5 s ({p['dur']:.3f})")
            check(p["audio"] == "aac", f"audio is kept ({p['audio']})")
        else:
            check(False, "output file exists")
        check(r.get("cancelStatus") == "export cancelled", f"cancelling stops the export ({r.get('cancelStatus')!r})")
        check(not glob.glob(os.path.join(tmp, "out", ".*kroppi-part")), "a cancelled export leaves no partial file")
        check(os.path.isfile(out) and probe(out)["w"] == 960, "a cancelled export doesn't touch the earlier file")
        check(not r.get("errors"), f"no JavaScript errors {r.get('errors') or ''}")

        print("fast")
        out = os.path.join(tmp, "out", "fast.mp4")
        t0 = time.time()
        _, res = run("fast", mp4, tmp, out)
        r = result(res)
        check("copy" in r.get("fmt", ""), f"summary says copy ({r.get('fmt')})")
        check("lossless copy" in r.get("meta", ""), f"export used fast mode: {r.get('meta')}")
        if os.path.isfile(out):
            p = probe(out)
            check(p["w"] == 1280 and p["h"] == 720 and p["vcodec"] == "h264", f"streams copied ({p['vcodec']} {p['w']}x{p['h']})")
            check(1.9 < p["dur"] < 3.2, f"trimmed near 2 s, snapped to keyframes ({p['dur']:.3f})")
        else:
            check(False, "output file exists")
        check(r.get("modeWhileCropped") == "exact", "adding a crop switches to exact")
        check(not r.get("errors"), f"no JavaScript errors {r.get('errors') or ''}")

        print("proxy")
        out = os.path.join(tmp, "out", "from-avi.mp4")
        _, res = run("proxy", avi, tmp, out)
        r = result(res)
        check(r.get("vw") == 320 and r.get("vh") == 240 and r.get("container") == "mp4", f"AVI opened with a preview copy ({r.get('vw')}x{r.get('vh')}, saves as {r.get('container')})")
        if os.path.isfile(out):
            p = probe(out)
            check(p["w"] == 240 and p["h"] == 240 and "mp4" in (p["format"] or ""), f"exported square mp4 ({p['w']}x{p['h']}, {p['format']})")
            check(abs(p["dur"] - 1.5) < 0.1, f"trimmed to 1.5 s ({p['dur']:.3f})")
        else:
            check(False, "output file exists")
        check(not r.get("errors"), f"no JavaScript errors {r.get('errors') or ''}")

        print("quit")
        state = {}
        def during(proc, result):
            if result.get("ready"):
                close_window(proc.pid)
                time.sleep(2)
                state["alive_after_close"] = proc.poll() is None
        proc, res = run("quit", mp4, tmp, os.path.join(tmp, "out", "unused.mp4"), during)
        check(state.get("alive_after_close") is True, "closing with unsaved work doesn't quit straight away")
        check(any(r.get("prompt") for r in res), "the quit prompt appears")
        check(proc.returncode == 0, f"choosing quit closes the app (exit {proc.returncode})")
    print("\nall passed" if not failures else f"\n{failures} FAILED")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
