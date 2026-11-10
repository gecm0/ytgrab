// No console window on Windows; the app is GUI-only.
#![cfg_attr(windows, windows_subsystem = "windows")]

use gpui::{
    App, Application, Bounds, Context, Div, ElementId, FocusHandle, FontWeight, KeyBinding, MouseButton,
    PathPromptOptions, SharedString, Stateful, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, img,
    prelude::*, px, rgb, size, transparent_black,
};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PORT: u16 = 47321;
// Fixed by the "key" in extension/manifest.json. Web pages and other extensions cannot send this
// Origin, so it is the only check needed while the server can do no more than queue YouTube URLs.
const EXTENSION_ORIGIN: &str = "chrome-extension://nmiolemafidjgnibgjgjooceilfpdoaf";
const MAX_BODY: usize = 1 << 20; // cookies for youtube.com fit easily in 1 MB
// Same codec on every download: VP9 video + Opus audio, muxed into MKV.
const FORMAT: &str = "bv[vcodec~='^(vp9|vp09)']+ba[acodec=opus]";

actions!(ytgrab, [FocusNext, FocusPrev, Paste]);

/// Job ids for both the extension and pasted links; ids also name the cookie temp files.
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

fn next_id() -> usize {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

struct Job {
    id: usize,
    url: String,
    cookies: String,
}

#[derive(Clone, Copy)]
enum Kind {
    Busy,
    Ok,
    Warn,
}

/// One short line for the status bar; `detail` (full error text) shows behind "Details".
const FAILED: &str = "Download failed";
const RETRYING: &str = "Retrying…";
// secondary-v in the key bindings: Cmd on macOS, Ctrl elsewhere.
const PASTE_KEY: &str = if cfg!(target_os = "macos") { "⌘V" } else { "Ctrl+V" };

struct Status {
    kind: Kind,
    text: String,
    detail: String,
}

fn status(kind: Kind, text: impl Into<String>, detail: impl Into<String>) -> Status {
    Status { kind, text: text.into(), detail: detail.into() }
}

/// What a running job is doing. FORMAT always downloads the video first, then the audio, then merges.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Starting,
    Video,
    Audio,
    Merging,
}

enum Msg {
    Status(Status),
    New { id: usize, url: String, cookies: String },
    Title { id: usize, title: String },
    Thumb { id: usize, path: PathBuf },
    File { id: usize, path: PathBuf },
    Progress { id: usize, pct: f32, phase: Phase },
    Done { id: usize, result: Result<(), Failure> },
}

/// A failed job: `short` goes on the row, `detail` behind "Details" in the status bar.
#[derive(Debug)]
struct Failure {
    short: String,
    detail: String,
}

// ---------- paths ----------

fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf())).unwrap_or_else(|| PathBuf::from("."))
}

/// `bin/<name>` next to the app, or the bare name so PATH is used (handy on a dev machine).
fn tool(name: &str) -> PathBuf {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let bundled = exe_dir().join("bin").join(&file);
    if bundled.exists() { bundled } else { PathBuf::from(file) }
}

fn home() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    home.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

// ---------- settings ----------

/// %APPDATA%\ytgrab\settings.json on Windows, ~/.config/ytgrab/settings.json elsewhere.
fn settings_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home().join(".config"));
    base.join("ytgrab").join("settings.json")
}

fn load_download_dir() -> PathBuf {
    std::fs::read(settings_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["download_dir"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| home().join("Downloads"))
}

fn save_download_dir(dir: &Path) -> std::io::Result<()> {
    let path = settings_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, serde_json::json!({ "download_dir": dir }).to_string())
}

// ---------- request validation ----------

fn is_youtube_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    matches!(host, "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be")
}

/// Checks a POST /job request and returns (url, cookies) or an HTTP error.
fn parse_job(
    method: &str,
    path: &str,
    origin: Option<&str>,
    body: &[u8],
) -> Result<(String, String), (u16, &'static str)> {
    if method != "POST" || path != "/job" {
        return Err((404, "not found"));
    }
    if origin != Some(EXTENSION_ORIGIN) {
        return Err((403, "bad origin"));
    }
    let json: serde_json::Value = serde_json::from_slice(body).map_err(|_| (400, "bad json"))?;
    let url = json["url"].as_str().unwrap_or_default();
    if !is_youtube_url(url) {
        return Err((400, "not a youtube url"));
    }
    let cookies = json["cookies"].as_str().unwrap_or_default();
    Ok((url.to_string(), cookies.to_string()))
}

/// Clipboard text -> the YouTube URL in it, if that is all it holds.
fn pasted_url(text: &str) -> Option<&str> {
    let url = text.trim();
    (is_youtube_url(url) && !url.contains(char::is_whitespace)).then_some(url)
}

fn serve(server: tiny_http::Server, jobs: Sender<Job>, ui: Sender<Msg>) {
    for mut req in server.incoming_requests() {
        let origin = req.headers().iter().find(|h| h.field.equiv("Origin")).map(|h| h.value.as_str().to_string());
        let mut body = Vec::new();
        let too_big =
            req.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body).is_err() || body.len() > MAX_BODY;
        let result = if too_big {
            Err((413, "body too large"))
        } else {
            parse_job(req.method().as_str(), req.url(), origin.as_deref(), &body)
        };
        let (code, text) = match result {
            Ok((url, cookies)) => {
                let id = next_id();
                let _ = ui.send(Msg::New { id, url: url.clone(), cookies: cookies.clone() });
                let _ = jobs.send(Job { id, url, cookies });
                (202, "queued")
            }
            Err(e) => e,
        };
        let _ = req.respond(tiny_http::Response::from_string(text).with_status_code(code));
    }
}

// ---------- yt-dlp worker ----------

fn command(program: PathBuf) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))] // only mutated on Windows
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash per job
    }
    cmd
}

/// "[download]  42.3% of ..." -> 42.3
fn parse_pct(line: &str) -> Option<f32> {
    let rest = line.strip_prefix("[download]")?;
    rest.split_whitespace().next()?.strip_suffix('%')?.parse().ok()
}

/// Each file yt-dlp starts announces a "Destination" (or "has already been downloaded" when it is
/// on disk from an earlier try); the merge step prints "[Merger]".
fn next_phase(phase: Phase, line: &str) -> Phase {
    let new_file = line.starts_with("[download] Destination:")
        || (line.starts_with("[download] ") && line.ends_with("has already been downloaded"));
    if new_file {
        if phase == Phase::Video { Phase::Audio } else { Phase::Video }
    } else if line.starts_with("[Merger]") {
        Phase::Merging
    } else {
        phase
    }
}

/// "YTGRAB_INFO <id> <title>" printed by yt-dlp before downloading -> (id, title).
fn parse_info(line: &str) -> Option<(&str, &str)> {
    let (id, title) = line.strip_prefix("YTGRAB_INFO ")?.split_once(' ')?;
    // The id goes into a URL and a file name, so accept only YouTube's id alphabet.
    let valid = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    valid.then_some((id, title))
}

/// Fetches YouTube's 320x180 thumbnail with the system curl (built into Windows 10+ and macOS).
fn fetch_thumb(job_id: usize, video_id: &str, ui: &Sender<Msg>) {
    let path = std::env::temp_dir().join(format!("ytgrab-thumb-{video_id}.jpg"));
    let url = format!("https://i.ytimg.com/vi/{video_id}/mqdefault.jpg");
    let ui = ui.clone();
    std::thread::spawn(move || {
        let ok = command(PathBuf::from("curl"))
            .args(["-sfL", "-m", "15", "-o"])
            .arg(&path)
            .arg(url)
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            let _ = ui.send(Msg::Thumb { id: job_id, path });
        }
    });
}

/// yt-dlp's arguments. The URL must come last, after "--": anything after "--" is taken as a URL.
fn ytdlp_args(dir: &Path, cookie_file: Option<&Path>, ffmpeg: Option<&Path>, url: &str) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "-f",
        FORMAT,
        "--merge-output-format",
        "mkv",
        "--newline",
        "--no-playlist",
        "--embed-metadata",
        // --print would imply --quiet/--simulate; keep progress lines and the real download.
        "--no-quiet",
        "--no-simulate",
        // A Windows pipe gets the ANSI code page by default: accented titles turned into bytes that
        // are not UTF-8, which stopped our reader, closed the pipe, and made yt-dlp's next write fail
        // with "[Errno 22] Invalid argument". yt-dlp encodes all its output with this instead.
        "--encoding",
        "utf-8",
        "--print",
        "before_dl:YTGRAB_INFO %(id)s %(title)s",
        "--print",
        "after_move:YTGRAB_FILE %(filepath)s",
        "-o",
        "%(title)s [%(id)s].%(ext)s",
    ]
    .map(Into::into)
    .into();
    args.extend(["-P".into(), dir.into()]);
    if let Some(cookie_file) = cookie_file {
        args.extend(["--cookies".into(), cookie_file.into()]);
    }
    if let Some(ffmpeg) = ffmpeg {
        args.extend(["--ffmpeg-location".into(), ffmpeg.into()]);
    }
    // The URL comes from an HTTP body; "--" keeps it from ever being parsed as a flag.
    args.extend(["--".into(), url.into()]);
    args
}

fn run_job(job: &Job, dir: &Path, version: &str, ui: &Sender<Msg>) -> Result<(), Failure> {
    let cookie_file = std::env::temp_dir().join(format!("ytgrab-cookies-{}.txt", job.id));
    // A pasted link before any extension job has no cookies; yt-dlp rejects an empty cookies file.
    let use_cookies = !job.cookies.is_empty();
    if use_cookies {
        std::fs::write(&cookie_file, &job.cookies).map_err(|e| {
            // A partial write (disk full) can still hold live cookies: delete it here too.
            let _ = std::fs::remove_file(&cookie_file);
            Failure {
                short: "Cannot write a temporary file. Check free space on the system drive.".into(),
                detail: format!("Writing {}: {e}", cookie_file.display()),
            }
        })?;
    }

    // Bundled ffmpeg lives in bin/ too; yt-dlp only searches PATH by default.
    let ffmpeg = Some(tool("ffmpeg")).filter(|p| p.is_absolute());
    let mut cmd = command(tool("yt-dlp"));
    cmd.args(ytdlp_args(dir, use_cookies.then_some(cookie_file.as_path()), ffmpeg.as_deref(), &job.url))
        .stdin(Stdio::null()) // GUI app has no console; don't hand Python a dead stdin
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let result = (|| {
        let mut child = cmd.spawn().map_err(|e| spawn_failure(&e))?;
        let stderr = child.stderr.take().unwrap();
        let err_reader = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = BufReader::new(stderr).read_to_end(&mut b);
            String::from_utf8_lossy(&b).into_owned()
        });
        let mut phase = Phase::Starting;
        // Read to EOF whatever the bytes are: stopping early closes the pipe under yt-dlp.
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut buf = Vec::new();
        while out.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
            let line = String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_string();
            buf.clear();
            let next = next_phase(phase, &line);
            if next == Phase::Merging && phase != Phase::Merging {
                let _ = ui.send(Msg::Progress { id: job.id, pct: 100., phase: next });
            }
            phase = next;
            if let Some(pct) = parse_pct(&line) {
                // A file that was already on disk prints progress without a Destination line.
                if phase == Phase::Starting {
                    phase = Phase::Video;
                }
                let _ = ui.send(Msg::Progress { id: job.id, pct, phase });
            }
            if let Some((video_id, title)) = parse_info(&line) {
                let _ = ui.send(Msg::Title { id: job.id, title: title.to_string() });
                fetch_thumb(job.id, video_id, ui);
            }
            if let Some(path) = line.strip_prefix("YTGRAB_FILE ") {
                let _ = ui.send(Msg::File { id: job.id, path: PathBuf::from(path) });
            }
        }
        let status = child.wait();
        let stderr = err_reader.join().unwrap_or_default();
        let code = match status {
            Ok(s) if s.success() => return Ok(()),
            Ok(s) => s.code().map_or("none (killed)".to_string(), |c| c.to_string()),
            Err(e) => format!("unknown ({e})"),
        };
        let short = scrub_cookies(&explain(&stderr), &job.cookies);
        Err(Failure { short, detail: details(&stderr, &code, dir, version, &job.cookies) })
    })();

    // Cookies are credentials: always delete, whatever happened.
    let _ = std::fs::remove_file(&cookie_file);
    result
}

fn spawn_failure(e: &std::io::Error) -> Failure {
    let short = match e.kind() {
        std::io::ErrorKind::NotFound => "yt-dlp is missing. Put it in bin/ next to the app.",
        _ => "Cannot start yt-dlp. See Details.",
    };
    Failure { short: short.into(), detail: format!("Cannot start {}: {e}", tool("yt-dlp").display()) }
}

/// One plain-English line for the row: what went wrong and what to do. Matches yt-dlp's ERROR lines
/// (or the whole output when a crash left none) against the usual causes.
fn explain(stderr: &str) -> String {
    let errors: Vec<&str> = stderr.lines().filter(|l| l.starts_with("ERROR")).collect();
    let text = if errors.is_empty() { stderr.to_string() } else { errors.join("\n") }.to_lowercase();
    let has = |pats: &[&str]| pats.iter().any(|p| text.contains(p));
    let msg = if has(&["ffmpeg is not installed", "ffmpeg not found"]) {
        "ffmpeg is missing. Put ffmpeg in bin/ next to the app."
    } else if has(&["no space left", "errno 28", "winerror 112"]) {
        "The disk is full. Free some space or pick another folder with Change…"
    } else if has(&["permission denied", "errno 13", "winerror 5]"]) {
        "No permission to write to the download folder. Pick another folder with Change…"
    } else if has(&["errno 36", "winerror 206", "file name too long"]) {
        "The file path is too long. Pick a shorter folder with Change…"
    } else if has(&["errno 22", "invalid argument"]) {
        "Windows refused a file write (invalid argument). Retry; if it fails again, send screenshots of Details (scroll to the end)."
    } else if has(&["not a bot"]) {
        "YouTube asked to confirm you are not a bot. Sign in to YouTube in Chrome, wait a while, then click Download on the video in Chrome so ytgrab gets your login."
    } else if has(&["confirm your age", "age-restricted", "inappropriate for some users"]) {
        "This video has an age check. Sign in to YouTube in Chrome, then click Download on the video in Chrome so ytgrab gets your login."
    } else if has(&["private video"]) {
        "This video is private."
    } else if has(&["members-only", "join this channel", "sign in"]) {
        "This video needs you signed in. Sign in to YouTube in Chrome, then click Download on the video in Chrome so ytgrab gets your login."
    } else if has(&["video unavailable", "video is unavailable", "has been removed", "not available in your country"]) {
        "This video is unavailable (removed, blocked or not available in your country)."
    } else if has(&["requested format is not available"]) {
        "This video has no VP9 + Opus version, which ytgrab needs."
    } else if has(&["http error 403"]) {
        // Before the network branch: "Unable to download webpage: HTTP Error 403" is not a network problem.
        "YouTube refused the download (HTTP 403). Restart ytgrab to update yt-dlp, then Retry."
    } else if has(&[
        "unable to download webpage",
        "getaddrinfo",
        "timed out",
        "connection reset",
        "connection refused",
        "network is unreachable",
        "temporary failure in name resolution",
        "http error 5",
    ]) {
        "Network problem. Check your connection, then Retry."
    } else if has(&["yt-dlp -u", "nsig", "precondition check failed", "unable to extract"]) {
        "yt-dlp may be out of date. Restart ytgrab to update it, then Retry."
    } else {
        // Unknown: the raw ERROR line, else the last line (the exception of a Python traceback).
        let last = errors.last().copied().or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()));
        return last.map_or("yt-dlp failed with no message".into(), |l| l.trim_start_matches("ERROR: ").to_string());
    };
    msg.to_string()
}

/// The text behind "Details": exit code, version, folder and the last lines yt-dlp printed on
/// stderr. Never shows cookie values.
fn details(stderr: &str, code: &str, dir: &Path, version: &str, cookies: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(20)..].join("\n");
    let text =
        format!("Exit code {code} · yt-dlp {version} · Folder {}\nLast lines from yt-dlp:\n{tail}", dir.display());
    scrub_cookies(&text, cookies)
}

/// Hides cookie values: Netscape cookie lines (tab-separated), Cookie headers, the bad lines yt-dlp
/// echoes in "skipping cookie file entry ... : '<line>'", and every value from this job's cookie file.
fn scrub_cookies(text: &str, cookies: &str) -> String {
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            if l.contains('\t') {
                return "[line hidden: may hold cookies]".to_string();
            }
            if let Some(i) = l.find("cookie file entry") {
                return format!("{}cookie file entry [hidden]", &l[..i]);
            }
            // ASCII lowercase keeps byte offsets valid for &l[..i].
            match l.to_ascii_lowercase().find("cookie:") {
                Some(i) => format!("{}cookie: [hidden]", &l[..i]),
                None => l.to_string(),
            }
        })
        .collect();
    // ponytail: values under 8 chars ("1", "en") are left alone; they are not secrets and replacing
    // them would mangle ordinary text.
    // The last field, not the 7th: malformed lines (the ones yt-dlp complains about) have fewer.
    let values =
        cookies.lines().filter(|l| l.contains('\t')).filter_map(|l| l.rsplit('\t').next()).filter(|v| v.len() >= 8);
    for v in values {
        for line in &mut out {
            *line = line.replace(v, "[hidden]");
        }
    }
    out.join("\n")
}

/// One worker, one job at a time. Updating first means yt-dlp.exe is never replaced while running.
fn work(jobs: Receiver<Job>, dir: Arc<Mutex<PathBuf>>, ui: Sender<Msg>) {
    // Leftovers from a previous run: cookie files hold live session cookies, thumbnails are cache.
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("ytgrab-cookies-") || name.starts_with("ytgrab-thumb-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let _ = ui.send(Msg::Status(status(Kind::Busy, "Updating yt-dlp…", "")));
    let update = command(tool("yt-dlp")).arg("-U").stdin(Stdio::null()).output();
    let msg = match update {
        Ok(out) if out.status.success() => status(Kind::Ok, "yt-dlp is up to date", ""),
        Ok(out) => status(
            Kind::Warn,
            "yt-dlp update failed, using the current version",
            String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""),
        ),
        Err(e) => status(Kind::Warn, "yt-dlp not found. Put it in bin/ next to the app.", e.to_string()),
    };
    let _ = ui.send(Msg::Status(msg));
    // Once per start, for Details when a job fails.
    let version = command(tool("yt-dlp"))
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|e| format!("unknown ({e})"));

    for job in jobs {
        let dir = dir.lock().unwrap().clone(); // folder at the moment the job starts
        let result = run_job(&job, &dir, &version, &ui);
        let _ = ui.send(Msg::Done { id: job.id, result });
    }
}

// ---------- UI ----------

enum State {
    Queued,
    Running(f32, Phase),
    Done,
    Failed(String),
}

struct Row {
    id: usize,
    url: String,
    cookies: String, // kept for Retry; cleared once the download succeeds
    title: String,
    thumb: Option<PathBuf>,
    file: Option<PathBuf>,
    state: State,
    detail: String, // Details text of the last failure, so Retry and other jobs don't lose it
}

struct Main {
    status: Status,
    details_open: bool,
    dir: Arc<Mutex<PathBuf>>,
    rows: Vec<Row>,
    jobs: Sender<Job>,
    /// False when the port was taken: then no worker runs and nothing may be queued.
    working: bool,
    /// The cookies of the extension's latest job, reused for pasted links. Memory only.
    last_cookies: String,
    /// Result of the last paste, shown under the header for a few seconds: (color, text).
    note: Option<(u32, &'static str)>,
    /// Bumped per note so an old timer doesn't clear a newer note.
    note_gen: usize,
    focus: FocusHandle,
    /// Focus ring only after Tab, like CSS :focus-visible; a mouse click hides it again.
    ring: bool,
}

// Palette from docs/ui.md.
const BG: u32 = 0x1c1c1e;
const RAISED: u32 = 0x2a2a2c;
const RAISED_HOVER: u32 = 0x353538;
const LINE: u32 = 0x2c2c2e;
const TEXT: u32 = 0xececee;
const MUTED: u32 = 0x9a9aa0;
const BLUE: u32 = 0x6cb6ff;
const GREEN: u32 = 0x57d37a;
const RED: u32 = 0xff6b6b;
const AMBER: u32 = 0xe8b45a;
const WARN_TEXT: u32 = 0xf0cf96;

// gpui panics if hover/active are set twice, so each button kind sets them exactly once.
// tab_index(0) makes the button a tab stop in layout order; gpui clicks a focused one on Enter/Space.
fn button_base(id: impl Into<ElementId>, label: &'static str, ring: bool) -> Stateful<Div> {
    div()
        .id(id)
        .tab_index(0)
        .border_1()
        .border_color(transparent_black())
        .when(ring, |d| d.focus(|s| s.border_color(rgb(BLUE))))
        .flex()
        .flex_none()
        .items_center()
        .h(px(28.))
        .px(px(10.))
        .rounded(px(6.))
        .whitespace_nowrap()
        .cursor_pointer()
        .child(label)
}

/// Primary button: raised fill; pressing drops it back from the hover shade.
fn button(id: impl Into<ElementId>, label: &'static str, ring: bool) -> Stateful<Div> {
    button_base(id, label, ring)
        .bg(rgb(RAISED))
        .text_color(rgb(TEXT))
        .hover(|s| s.bg(rgb(RAISED_HOVER)))
        .active(|s| s.bg(rgb(RAISED)))
}

/// Secondary button: no fill until hovered.
fn ghost(id: impl Into<ElementId>, label: &'static str, ring: bool) -> Stateful<Div> {
    button_base(id, label, ring)
        .text_color(rgb(MUTED))
        .hover(|s| s.bg(rgb(RAISED)).text_color(rgb(TEXT)))
        .active(|s| s.bg(rgb(RAISED_HOVER)).text_color(rgb(TEXT)))
}

/// Hover hint for a control whose label doesn't say what it does.
struct Tip(&'static str);

impl Render for Tip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let tip = div().px_2().py_1().rounded(px(6.)).border_1().border_color(rgb(LINE)).bg(rgb(RAISED));
        tip.text_xs().text_color(rgb(TEXT)).child(self.0)
    }
}

/// /Users/me/Downloads -> ~/Downloads
fn tilde(p: &Path) -> String {
    match p.strip_prefix(home()) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        _ => p.display().to_string(),
    }
}

impl Main {
    fn apply(&mut self, msg: Msg) {
        if let Msg::Status(s) = msg {
            self.status = s;
            return;
        }
        if let Msg::New { id, url, cookies } = msg {
            self.last_cookies = cookies.clone();
            self.rows.push(Row {
                id,
                title: url.clone(),
                url,
                cookies,
                thumb: None,
                file: None,
                state: State::Queued,
                detail: String::new(),
            });
            return;
        }
        let id = match &msg {
            Msg::Title { id, .. } | Msg::Thumb { id, .. } | Msg::File { id, .. } => *id,
            Msg::Progress { id, .. } | Msg::Done { id, .. } => *id,
            Msg::Status(_) | Msg::New { .. } => unreachable!(),
        };
        let Some(r) = self.rows.iter_mut().find(|r| r.id == id) else { return };
        match msg {
            Msg::Title { title, .. } => {
                r.title = title;
                // yt-dlp is working on it now, even before the first percentage.
                if matches!(r.state, State::Queued) {
                    r.state = State::Running(0., Phase::Starting);
                }
            }
            Msg::Thumb { path, .. } => r.thumb = Some(path),
            Msg::File { path, .. } => r.file = Some(path),
            Msg::Progress { pct, phase, .. } => r.state = State::Running(pct, phase),
            Msg::Done { result: Ok(()), .. } => {
                r.state = State::Done;
                r.cookies = String::new(); // release the buffer, not just its contents
                r.detail = String::new();
                // Clear the amber bar once things work again, but keep it (and its Details) while
                // another failed row is still on screen.
                let failed = self.rows.iter().rev().find(|r| matches!(r.state, State::Failed(_)));
                if matches!(self.status.text.as_str(), FAILED | RETRYING) {
                    self.status = match failed {
                        Some(f) => status(Kind::Warn, FAILED, f.detail.clone()),
                        None => status(Kind::Ok, "Download finished", ""),
                    };
                }
            }
            Msg::Done { result: Err(f), .. } => {
                // The row shows the short line; Details in the status bar shows the raw context.
                self.status = status(Kind::Warn, FAILED, f.detail.clone());
                r.detail = f.detail;
                r.state = State::Failed(f.short);
            }
            Msg::Status(_) | Msg::New { .. } => {}
        }
    }

    fn retry(&mut self, id: usize) {
        let Some(r) = self.rows.iter_mut().find(|r| r.id == id) else { return };
        r.state = State::Queued;
        if self.status.text == FAILED {
            // Keep this row's Details until the retry has its own result.
            self.status = status(Kind::Busy, RETRYING, r.detail.clone());
        }
        let _ = self.jobs.send(Job { id, url: r.url.clone(), cookies: r.cookies.clone() });
    }

    /// Queues the link on the clipboard the same way the extension's request is queued.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
        let note = match pasted_url(&text) {
            // No worker without the port, so a job would sit in "Queued" forever.
            _ if !self.working => (AMBER, "Close the other ytgrab to add links."),
            None => (AMBER, "No YouTube link copied."),
            Some(url) => {
                let (id, url, cookies) = (next_id(), url.to_string(), self.last_cookies.clone());
                let _ = self.jobs.send(Job { id, url: url.clone(), cookies: cookies.clone() });
                self.apply(Msg::New { id, url, cookies });
                (GREEN, "Added to the queue.")
            }
        };
        self.note = Some(note);
        self.note_gen += 1;
        let gen_ = self.note_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            let _ = this.update(cx, |main: &mut Main, cx| {
                if main.note_gen == gen_ {
                    main.note = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_dir(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use this folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(dir) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |main: &mut Main, cx| {
                main.status = match save_download_dir(&dir) {
                    Ok(()) => status(Kind::Ok, format!("Downloads will go to {}", tilde(&dir)), ""),
                    Err(e) => {
                        status(Kind::Warn, "Folder changed for this session only", format!("Cannot save settings: {e}"))
                    }
                };
                *main.dir.lock().unwrap() = dir;
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for Main {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dir = self.dir.lock().unwrap().clone();
        let ring = self.ring;
        // The paste hint, or the last paste's result for 4 s. One truncated line beside the button,
        // so neither the button nor the list moves when the text changes.
        let (note_color, note_text) = match self.note {
            Some((color, text)) => (color, SharedString::from(text)),
            None => (MUTED, format!("or press {PASTE_KEY} anywhere").into()),
        };
        let any_done = self.rows.iter().any(|r| matches!(r.state, State::Done));
        let running = self.rows.iter().filter(|r| matches!(r.state, State::Running(..))).count();
        let queued = self.rows.iter().filter(|r| matches!(r.state, State::Queued)).count();
        let summary =
            [(running > 0).then(|| format!("{running} downloading")), (queued > 0).then(|| format!("{queued} queued"))]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .child(
                button_base("paste", "Add from clipboard", ring)
                    .gap_2()
                    .bg(rgb(RAISED))
                    .text_color(rgb(TEXT))
                    .hover(|s| s.bg(rgb(RAISED_HOVER)))
                    .active(|s| s.bg(rgb(RAISED)))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(PASTE_KEY))
                    .on_click(cx.listener(|this, _, _, cx| this.paste(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(rgb(note_color))
                    .text_ellipsis()
                    .line_clamp(1)
                    .child(note_text),
            )
            .child(div().flex_none().text_xs().text_color(rgb(MUTED)).child(summary))
            .when(any_done, |d| {
                d.child(ghost("clear-done", "Clear completed", ring).h(px(24.)).px_2().text_xs().on_click(cx.listener(
                    |this, _, window, cx| {
                        window.focus(&this.focus);
                        this.rows.retain(|r| !matches!(r.state, State::Done));
                        cx.notify();
                    },
                )))
            });

        let rows = self.rows.iter().rev().map(|r| {
            // 16:9 preview; grey box until the thumbnail arrives.
            let thumb = div().w(px(112.)).h(px(63.)).flex_none().rounded(px(6.)).overflow_hidden().bg(rgb(RAISED));
            let thumb = match &r.thumb {
                Some(p) => thumb.child(img(p.clone()).size_full()),
                None => thumb,
            };
            // Until yt-dlp reports the title, the "title" is the raw URL: keep it quiet.
            let title =
                div().text_size(px(14.)).line_height(px(20.)).text_ellipsis().line_clamp(1).child(r.title.clone());
            let title = match r.state {
                State::Queued => title.text_color(rgb(MUTED)),
                _ => title.font_weight(FontWeight::MEDIUM),
            };
            let (text, color) = match &r.state {
                State::Queued => ("Queued".to_string(), MUTED),
                State::Running(_, Phase::Starting) => ("Starting…".to_string(), BLUE),
                State::Running(p, Phase::Video) => (format!("Downloading video {p:.1}%"), BLUE),
                State::Running(p, Phase::Audio) => (format!("Downloading audio {p:.1}%"), BLUE),
                State::Running(_, Phase::Merging) => ("Merging video and audio…".to_string(), BLUE),
                State::Done => ("Done".to_string(), GREEN),
                State::Failed(e) => (format!("Failed: {e}"), RED),
            };
            let meta = div().text_xs().text_color(rgb(color)).child(text);
            let meta = if matches!(r.state, State::Failed(_)) {
                meta.text_ellipsis().line_clamp(2)
            } else {
                meta.text_ellipsis().line_clamp(1)
            };
            // The bar's slot is always there (empty unless running), so the text never moves when it appears.
            let bar = div().mt_0p5().h_1().w_full().rounded_full();
            let bar = match r.state {
                State::Running(p, _) => {
                    bar.bg(rgb(RAISED)).child(div().h_full().rounded_full().w(gpui::relative(p / 100.0)).bg(rgb(BLUE)))
                }
                _ => bar,
            };
            let body = div().flex().flex_col().flex_1().min_w_0().gap_1().child(title).child(meta).child(bar);
            let mut actions = div().flex().flex_none().gap_1();
            let id = r.id;
            let remove = || {
                ghost(("remove", id), "Remove", ring).on_click(cx.listener(move |this, _, window, cx| {
                    // This button disappears; keep focus inside the window so Tab still works.
                    window.focus(&this.focus);
                    this.rows.retain(|r| r.id != id);
                    cx.notify();
                }))
            };
            match (&r.state, &r.file) {
                (State::Done, Some(file)) => {
                    let (play, reveal) = (file.clone(), file.clone());
                    actions = actions
                        .child(button(("play", id), "Play", ring).on_click(move |_, _, cx| cx.open_with_system(&play)))
                        .child(
                            ghost(("reveal", id), "Show in folder", ring)
                                .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
                        )
                        .child(remove());
                }
                (State::Done, None) => actions = actions.child(remove()),
                (State::Failed(_), _) => {
                    actions = actions
                        .child(button(("retry", id), "Retry", ring).on_click(cx.listener(
                            move |this, _, window, cx| {
                                window.focus(&this.focus);
                                this.retry(id);
                                cx.notify();
                            },
                        )))
                        .child(remove());
                }
                _ => {}
            }
            div()
                .flex()
                .items_center()
                .gap(px(14.))
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(rgb(LINE))
                .child(thumb)
                .child(body)
                .child(actions)
        });

        let list = if self.rows.is_empty() {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_1()
                .p_6()
                .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child("No downloads yet"))
                .child(div().max_w(px(340.)).text_center().text_color(rgb(MUTED)).child(
                    "Copy a YouTube link and press Add from clipboard, or click Download under a video in Chrome.",
                ))
                .into_any_element()
        } else {
            div().id("jobs").flex().flex_col().flex_1().overflow_y_scroll().children(rows).into_any_element()
        };

        let s = &self.status;
        let dot = match s.kind {
            Kind::Busy => BLUE,
            Kind::Ok => GREEN,
            Kind::Warn => AMBER,
        };
        let has_detail = !s.detail.is_empty();
        let open = self.details_open && has_detail;
        let status_bar = div()
            .flex_none()
            .border_t_1()
            .border_color(rgb(LINE))
            .when(open, |d| {
                // ~25 lines of yt-dlp output: cap the height so the list stays visible, scroll the rest.
                d.child(
                    div()
                        .id("detail")
                        .max_h(px(200.))
                        .overflow_y_scroll()
                        .px_4()
                        .pt(px(10.))
                        .text_xs()
                        .text_color(rgb(WARN_TEXT))
                        .child(s.detail.clone()),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(32.))
                    .px_4()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    // Status left (natural width, at most 55%), folder right (the rest). Tab order follows
                    // layout: Add, Clear completed, row buttons, Details, folder, Change….
                    // Here and in the rows, line_clamp + text_ellipsis instead of truncate(): gpui 0.2.2
                    // caches a nowrap text's first measurement, so truncate() in a flex row clips without "…".
                    .child(
                        div()
                            .flex()
                            .flex_initial()
                            .max_w(gpui::relative(0.55))
                            .min_w_0()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(6.)).flex_none().rounded_full().bg(rgb(dot)))
                            .child(div().min_w_0().text_ellipsis().line_clamp(1).child(s.text.clone()))
                            .when(has_detail, |d| {
                                d.child(
                                    // Fixed width: "Details" -> "Hide" must not move the folder.
                                    ghost("details", if open { "Hide" } else { "Details" }, ring)
                                        .h(px(24.))
                                        .w(px(56.))
                                        .justify_center()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.details_open = !this.details_open;
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .justify_end()
                            .gap_1()
                            // The download folder itself is the "open folder" control: a ghost button
                            // whose label is the path, so it shrinks and truncates instead of a fixed label.
                            .child(
                                div()
                                    .id("open-dir")
                                    .tab_index(0)
                                    .border_1()
                                    .border_color(transparent_black())
                                    .when(ring, |d| d.focus(|s| s.border_color(rgb(BLUE))))
                                    .flex()
                                    .flex_initial()
                                    .min_w_0()
                                    .items_center()
                                    .h(px(24.))
                                    .px_2()
                                    .rounded(px(6.))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(RAISED)).text_color(rgb(TEXT)))
                                    .active(|s| s.bg(rgb(RAISED_HOVER)).text_color(rgb(TEXT)))
                                    .tooltip(|_, cx| cx.new(|_| Tip("Open folder")).into())
                                    .child(div().min_w_0().text_ellipsis().line_clamp(1).child(tilde(&dir)))
                                    .on_click(move |_, _, cx| cx.open_with_system(&dir)),
                            )
                            .child(
                                ghost("change-dir", "Change…", ring)
                                    .h(px(24.))
                                    .px_2()
                                    .on_click(cx.listener(|this, _, _, cx| this.choose_dir(cx))),
                            ),
                    ),
            );

        div()
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &FocusNext, window, cx| {
                window.focus_next();
                this.ring = true;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
            .on_action(cx.listener(|this, _: &FocusPrev, window, cx| {
                window.focus_prev();
                this.ring = true;
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.ring {
                        this.ring = false;
                        cx.notify();
                    }
                }),
            )
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_size(px(13.))
            .child(header)
            .child(list)
            .child(status_bar)
    }
}

fn main() {
    let dir = Arc::new(Mutex::new(load_download_dir()));
    let (ui_tx, ui_rx) = mpsc::channel::<Msg>();
    let (job_tx, job_rx) = mpsc::channel::<Job>();

    // Bind before starting anything else: without the port no job can ever arrive, so this warning
    // must stay on screen instead of being replaced by the worker's yt-dlp update status.
    let (first_status, working) = match tiny_http::Server::http(("127.0.0.1", PORT)) {
        Ok(server) => {
            let (ui, jobs) = (ui_tx.clone(), job_tx.clone());
            std::thread::spawn(move || serve(server, jobs, ui));
            let dir = dir.clone();
            std::thread::spawn(move || work(job_rx, dir, ui_tx));
            (status(Kind::Busy, "Starting…", ""), true)
        }
        Err(e) => {
            let text = format!("Cannot listen on 127.0.0.1:{PORT}. Is ytgrab already open?");
            (status(Kind::Warn, text, e.to_string()), false)
        }
    };

    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("tab", FocusNext, None),
            KeyBinding::new("shift-tab", FocusPrev, None),
            // secondary = Cmd on macOS, Ctrl elsewhere.
            KeyBinding::new("secondary-v", Paste, None),
        ]);
        let bounds = Bounds::centered(None, size(px(720.), px(520.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("ytgrab".into()), ..Default::default() }),
                // Below this a finished row's three buttons squeeze the title to nothing.
                window_min_size: Some(size(px(560.), px(320.))),
                ..Default::default()
            },
            move |window, cx| {
                let main = cx.new(|cx| {
                    // ponytail: poll the channel every 200 ms; switch to an async channel if
                    // this ever needs lower latency than a progress bar does.
                    cx.spawn(async move |this, cx| {
                        loop {
                            cx.background_executor().timer(Duration::from_millis(200)).await;
                            let msgs: Vec<Msg> = ui_rx.try_iter().collect();
                            if msgs.is_empty() {
                                continue;
                            }
                            let alive = this.update(cx, |main: &mut Main, cx| {
                                msgs.into_iter().for_each(|m| main.apply(m));
                                cx.notify();
                            });
                            if alive.is_err() {
                                break;
                            }
                        }
                    })
                    .detach();
                    Main {
                        status: first_status,
                        details_open: false,
                        dir,
                        rows: Vec::new(),
                        jobs: job_tx,
                        working,
                        last_cookies: String::new(),
                        note: None,
                        note_gen: 0,
                        focus: cx.focus_handle(),
                        ring: false,
                    }
                });
                // Key bindings only fire inside a focused element, so focus the root from the start.
                let focus = main.read(cx).focus.clone();
                window.focus(&focus);
                main
            },
        )
        .unwrap();
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_and_name() {
        assert_eq!(parse_pct("[download]  42.3% of ~ 10.00MiB at 1.00MiB/s"), Some(42.3));
        assert_eq!(parse_pct("[download] Destination: a.webm"), None);
        assert_eq!(parse_pct("[youtube] abc: Downloading"), None);
        assert_eq!(parse_info("YTGRAB_INFO jNQXAC9IVRw Me at the zoo"), Some(("jNQXAC9IVRw", "Me at the zoo")));
        assert_eq!(parse_info("YTGRAB_INFO ../../x title"), None);
        assert_eq!(parse_info("[info] something"), None);
    }

    #[test]
    fn phases() {
        let mut p = Phase::Starting;
        for (line, want) in [
            ("[youtube] abc: Downloading webpage", Phase::Starting),
            ("[download] Destination: a.f303.webm", Phase::Video),
            ("[download]  50.0% of 10MiB", Phase::Video),
            ("[download] Destination: a.f251.webm", Phase::Audio),
            ("[Merger] Merging formats into \"a.mkv\"", Phase::Merging),
        ] {
            p = next_phase(p, line);
            assert_eq!(p, want, "{line}");
        }
        // A retry where both files are already on disk.
        let mut p = Phase::Starting;
        for (line, want) in [
            ("[download] a.f303.webm has already been downloaded", Phase::Video),
            ("[download] 100% of 10MiB", Phase::Video),
            ("[download] a.f251.webm has already been downloaded", Phase::Audio),
        ] {
            p = next_phase(p, line);
            assert_eq!(p, want, "{line}");
        }
    }

    #[test]
    fn home_shown_as_tilde() {
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(tilde(&home().join("Downloads")), format!("~{sep}Downloads"));
        assert_eq!(tilde(&home()), home().display().to_string());
        assert_eq!(tilde(Path::new("/elsewhere/x")), "/elsewhere/x");
    }

    #[test]
    fn settings_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("ytgrab-test-{}", std::process::id()));
        // SAFETY: the only test that touches env vars.
        unsafe { std::env::set_var("APPDATA", &tmp) };
        assert_eq!(load_download_dir(), home().join("Downloads")); // default when no file
        save_download_dir(Path::new("/some/where")).unwrap();
        assert_eq!(load_download_dir(), PathBuf::from("/some/where"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn url_is_the_last_argument() {
        let ffmpeg = Path::new("C:/ytgrab/bin/ffmpeg.exe");
        let cookies = Path::new("c.txt");
        for (ff, c) in [(Some(ffmpeg), Some(cookies)), (None, Some(cookies)), (Some(ffmpeg), None), (None, None)] {
            let args = ytdlp_args(Path::new("D:/Descargas"), c, ff, "https://youtu.be/abc");
            let n = args.len();
            assert_eq!(args[n - 2], "--");
            assert_eq!(args[n - 1], "https://youtu.be/abc");
            assert_eq!(args.iter().filter(|a| *a == "--").count(), 1);
            assert_eq!(args.iter().any(|a| a == "--ffmpeg-location"), ff.is_some());
            // yt-dlp rejects an empty cookies file, so no cookies means no --cookies at all.
            assert_eq!(args.iter().any(|a| a == "--cookies"), c.is_some());
        }
    }

    #[test]
    fn pasted_links() {
        assert_eq!(pasted_url("  https://youtu.be/abc\n"), Some("https://youtu.be/abc"));
        assert_eq!(pasted_url("https://www.youtube.com/watch?v=abc"), Some("https://www.youtube.com/watch?v=abc"));
        assert_eq!(pasted_url("www.youtube.com/watch?v=abc"), None);
        assert_eq!(pasted_url("https://www.youtube.com/watch?v=a https://evil.com"), None);
        assert_eq!(pasted_url("-o /tmp/x"), None);
        assert_eq!(pasted_url(""), None);
    }

    #[test]
    fn job_validation() {
        let ok_body = br#"{"url":"https://www.youtube.com/watch?v=abc","cookies":"x"}"#;
        let ext = Some(EXTENSION_ORIGIN);
        assert!(parse_job("POST", "/job", ext, ok_body).is_ok());
        assert_eq!(parse_job("GET", "/job", ext, ok_body).unwrap_err().0, 404);
        assert_eq!(parse_job("POST", "/job", Some("https://evil.com"), ok_body).unwrap_err().0, 403);
        assert_eq!(parse_job("POST", "/job", Some("chrome-extension://otherext"), ok_body).unwrap_err().0, 403);
        assert_eq!(parse_job("POST", "/job", None, ok_body).unwrap_err().0, 403);
        let bad_url = br#"{"url":"https://www.youtube.com.evil.com/x","cookies":""}"#;
        assert_eq!(parse_job("POST", "/job", ext, bad_url).unwrap_err().1, "not a youtube url");
        let flag = br#"{"url":"-o C:\\x","cookies":""}"#;
        assert!(parse_job("POST", "/job", ext, flag).is_err());
        assert!(is_youtube_url("https://youtu.be/abc"));
        assert!(!is_youtube_url("http://www.youtube.com/watch?v=a"));
    }

    // AGE, NETWORK and the unavailable/format/permission lines were captured from yt-dlp 2026.08.19;
    // the bot-check and private-video lines are the messages YouTube sends, as quoted in yt-dlp issues.
    const AGE: &str = "ERROR: [youtube] 07FYdnEawAQ: Sign in to confirm your age. Use --cookies-from-browser or --cookies for the authentication. See  https://github.com/yt-dlp/yt-dlp/wiki/FAQ#how-do-i-pass-cookies-to-yt-dlp  for how to manually pass cookies.";
    const NETWORK: &str = "ERROR: [youtube] 8YzunYUBQsg: Unable to download API page: ('Unable to connect to proxy', NewConnectionError(\"HTTPSConnection(host='127.0.0.1', port=9): Failed to establish a new connection: [Errno 61] Connection refused\")); please report this issue on  https://github.com/yt-dlp/yt-dlp/issues?q= , filling out the appropriate issue template. Confirm you are on the latest version using  yt-dlp -U";
    // The pipe failure as reproduced here (Errno 32 on macOS); Windows reports it as Errno 22.
    const ERRNO22: &str =
        "[youtube] 8YzunYUBQsg: Downloading webpage\nERROR: unable to open for writing: [Errno 22] Invalid argument";
    const ERRNO22_TRACEBACK: &str = "Traceback (most recent call last):\n  File \"yt_dlp\\__main__.py\", line 17, in <module>\n  File \"yt_dlp\\utils\\_utils.py\", line 1504, in write_string\nOSError: [Errno 22] Invalid argument";

    #[test]
    fn error_mapping() {
        for (stderr, want) in [
            (ERRNO22, "Windows refused a file write"),
            (ERRNO22_TRACEBACK, "Windows refused a file write"),
            (AGE, "age check"),
            (NETWORK, "Network problem"),
            ("ERROR: [youtube] aaaaaaaaaaa: This video is unavailable", "unavailable"),
            (
                "ERROR: [youtube] x: Sign in to confirm you\u{2019}re not a bot. Use --cookies-from-browser or --cookies for the authentication.",
                "not a bot",
            ),
            ("ERROR: [youtube] x: Private video. Sign in if you've been granted access to this video", "private"),
            (
                "ERROR: [youtube] 8YzunYUBQsg: Requested format is not available. Use --list-formats for a list of available formats",
                "VP9",
            ),
            ("ERROR: unable to open for writing: [Errno 13] Permission denied: '/tmp/ro/a.webm.part'", "No permission"),
            ("ERROR: unable to write data: [Errno 28] No space left on device", "disk is full"),
            (
                "ERROR: Postprocessing: ffmpeg not found. Please install or provide the path using --ffmpeg-location",
                "ffmpeg is missing",
            ),
            ("ERROR: unable to download video data: HTTP Error 403: Forbidden", "HTTP 403"),
            (
                "ERROR: [youtube] x: Unable to download webpage: HTTP Error 403: Forbidden (caused by <HTTPError 403: Forbidden>)",
                "HTTP 403",
            ),
            (
                "ERROR: [youtube] x: Unable to extract yt initial data; please report this issue on  https://github.com/yt-dlp/yt-dlp/issues?q= , filling out the appropriate issue template. Confirm you are on the latest version using  yt-dlp -U",
                "out of date",
            ),
        ] {
            let got = explain(stderr);
            assert!(got.contains(want), "{stderr}\n-> {got}");
        }
        // Retry reuses the row's old cookies (none for a pasted link), so sign-in fixes must point to
        // the Download button in Chrome, not to Retry.
        for m in [AGE, "ERROR: [youtube] x: Sign in to confirm you\u{2019}re not a bot.", "ERROR: x: Join this channel"]
        {
            assert!(explain(m).contains("click Download on the video in Chrome"), "{m}");
        }
        use std::io::{Error, ErrorKind};
        assert!(spawn_failure(&Error::from(ErrorKind::NotFound)).short.contains("missing"));
        let denied = spawn_failure(&Error::from(ErrorKind::PermissionDenied));
        assert!(denied.short.starts_with("Cannot start yt-dlp") && denied.detail.contains("ermission"), "{denied:?}");
        // Unknown errors fall back to the raw line, without the ERROR prefix.
        assert_eq!(explain("WARNING: a\nERROR: something new"), "something new");
        assert_eq!(explain("Traceback (most recent call last):\nValueError: boom\n"), "ValueError: boom");
        assert_eq!(explain(""), "yt-dlp failed with no message");
    }

    #[test]
    fn details_text() {
        let sid = "g.a000abcDEFsecretSIDvalue123";
        let cookies = format!(
            "# Netscape HTTP Cookie File\n#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1893456000\tSID\t{sid}\n.youtube.com\tTRUE\t/\tFALSE\t0\tPREF\tf6=8\n"
        );
        let mut stderr: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        // Real yt-dlp warning (cookies.py) for a malformed line: repr() turns its tabs into "\t".
        stderr += &format!(
            "WARNING: skipping cookie file entry due to invalid length 6: '.youtube.com\\tTRUE\\t/\\tFALSE\\tSAPISID\\tshort1'\n"
        );
        stderr += &format!(
            "[debug] Cookie: SID={sid}; PREF=f6=8\n.youtube.com\tTRUE\t/\tFALSE\t0\tPREF\tf6=8\nERROR: oops {sid}\n"
        );
        let d = details(&stderr, "1", Path::new("D:/Descargas"), "2026.08.19", &cookies);
        assert!(!d.contains(sid) && !d.contains("f6=8") && !d.contains("short1"), "{d}");
        assert!(d.starts_with("Exit code 1 · yt-dlp 2026.08.19 · Folder D:/Descargas\n"), "{d}");
        assert!(d.contains("WARNING: skipping cookie file entry [hidden]"));
        assert!(d.contains("ERROR: oops [hidden]"));
        assert!(!d.contains("line 14\n") && d.contains("line 15\n"), "keeps only the last 20 lines: {d}");
        // A traceback with no ERROR line is kept whole; Windows paths ("\t" in text) are not hidden.
        let t = details(ERRNO22_TRACEBACK, "1", Path::new("x"), "v", "");
        assert!(t.ends_with(ERRNO22_TRACEBACK), "{t}");
        let path = r"ERROR: unable to open for writing: [Errno 22] Invalid argument: 'D:\Descargas\tmp\a.part'";
        assert!(details(path, "1", Path::new("x"), "v", "").ends_with(path));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Real yt-dlp + network. Run with PYTHONIOENCODING=cp1252 to get the byte stream a Windows pipe gets:
    /// PYTHONIOENCODING=cp1252 cargo test live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn accented_title_with_ansi_stdout() {
        let dir = std::env::temp_dir().join(format!("ytgrab-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let job = Job {
            id: 999_999,
            url: "https://www.youtube.com/watch?v=8YzunYUBQsg".into(),
            cookies: "# Netscape HTTP Cookie File\n".into(),
        };
        let r = run_job(&job, &dir, "test", &tx);
        let msgs: Vec<Msg> = rx.try_iter().collect();
        let title = msgs.iter().find_map(|m| if let Msg::Title { title, .. } = m { Some(title.clone()) } else { None });
        let file = msgs.iter().find_map(|m| if let Msg::File { path, .. } = m { Some(path.clone()) } else { None });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_ok(), "{r:?}");
        assert!(title.unwrap_or_default().contains("canción"));
        assert!(file.is_some_and(|f| f.to_string_lossy().contains("canción")));
    }
}
