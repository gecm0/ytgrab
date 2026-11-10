// No console window on Windows; the app is GUI-only.
#![cfg_attr(windows, windows_subsystem = "windows")]

use gpui::{
    App, Application, Bounds, Context, Div, ElementId, FocusHandle, FontWeight, KeyBinding, MouseButton,
    PathPromptOptions, Stateful, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, img, prelude::*,
    px, rgb, size, transparent_black,
};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

actions!(ytgrab, [FocusNext, FocusPrev]);

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
    Done { id: usize, result: Result<(), String> },
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

fn serve(server: tiny_http::Server, jobs: Sender<Job>, ui: Sender<Msg>) {
    let mut next_id = 0;
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
                next_id += 1;
                let _ = ui.send(Msg::New { id: next_id, url: url.clone(), cookies: cookies.clone() });
                let _ = jobs.send(Job { id: next_id, url, cookies });
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

fn run_job(job: &Job, dir: &Path, ui: &Sender<Msg>) -> Result<(), String> {
    let cookie_file = std::env::temp_dir().join(format!("ytgrab-cookies-{}.txt", job.id));
    std::fs::write(&cookie_file, &job.cookies).map_err(|e| e.to_string())?;

    let mut cmd = command(tool("yt-dlp"));
    cmd.args(["-f", FORMAT, "--merge-output-format", "mkv", "--newline", "--no-playlist", "--embed-metadata"])
        // --print would imply --quiet/--simulate; keep progress lines and the real download.
        .args(["--no-quiet", "--no-simulate"])
        .args(["--print", "before_dl:YTGRAB_INFO %(id)s %(title)s"])
        .args(["--print", "after_move:YTGRAB_FILE %(filepath)s"])
        .arg("-P")
        .arg(dir)
        .args(["-o", "%(title)s [%(id)s].%(ext)s"])
        .arg("--cookies")
        .arg(&cookie_file)
        .arg("--") // the URL comes from an HTTP body; never let it be parsed as a flag
        .arg(&job.url)
        .stdin(Stdio::null()) // GUI app has no console; don't hand Python a dead stdin
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Bundled ffmpeg lives in bin/ too; yt-dlp only searches PATH by default.
    if let Some(ffmpeg) = Some(tool("ffmpeg")).filter(|p| p.is_absolute()) {
        cmd.arg("--ffmpeg-location").arg(ffmpeg);
    }

    let result = (|| {
        let mut child = cmd.spawn().map_err(|e| format!("cannot start yt-dlp: {e}"))?;
        let stderr = child.stderr.take().unwrap();
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut s);
            s
        });
        let mut phase = Phase::Starting;
        for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
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
        let status = child.wait().map_err(|e| e.to_string())?;
        let stderr = err_reader.join().unwrap_or_default();
        if status.success() {
            Ok(())
        } else {
            let last = stderr.lines().filter(|l| l.starts_with("ERROR")).last();
            Err(last.unwrap_or("yt-dlp failed").to_string())
        }
    })();

    // Cookies are credentials: always delete, whatever happened.
    let _ = std::fs::remove_file(&cookie_file);
    result
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

    for job in jobs {
        let dir = dir.lock().unwrap().clone(); // folder at the moment the job starts
        let result = run_job(&job, &dir, &ui);
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
}

struct Main {
    status: Status,
    details_open: bool,
    dir: Arc<Mutex<PathBuf>>,
    rows: Vec<Row>,
    jobs: Sender<Job>,
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
            self.rows.push(Row { id, title: url.clone(), url, cookies, thumb: None, file: None, state: State::Queued });
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
                // An older failure shouldn't keep the bar amber once things work again.
                if matches!(self.status.text.as_str(), FAILED | RETRYING) {
                    self.status = status(Kind::Ok, "Download finished", "");
                }
            }
            Msg::Done { result: Err(e), .. } => {
                // The row clamps the error to 2 lines; Details in the status bar shows all of it.
                self.status = status(Kind::Warn, FAILED, e.clone());
                r.state = State::Failed(e);
            }
            Msg::Status(_) | Msg::New { .. } => {}
        }
    }

    fn retry(&mut self, id: usize) {
        let Some(r) = self.rows.iter_mut().find(|r| r.id == id) else { return };
        r.state = State::Queued;
        if self.status.text == FAILED {
            self.status = status(Kind::Busy, RETRYING, "");
        }
        let _ = self.jobs.send(Job { id, url: r.url.clone(), cookies: r.cookies.clone() });
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
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_4()
            .py_3()
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(div().flex_none().text_color(rgb(MUTED)).child("Saving to"))
                    .child(div().min_w_0().truncate().child(tilde(&dir))),
            )
            .child(ghost("change-dir", "Change…", ring).on_click(cx.listener(|this, _, _, cx| this.choose_dir(cx))))
            .child(button("open-dir", "Open folder", ring).on_click(move |_, _, cx| cx.open_with_system(&dir)));

        let rows = self.rows.iter().rev().map(|r| {
            // 16:9 preview; grey box until the thumbnail arrives.
            let thumb = div().w(px(112.)).h(px(63.)).flex_none().rounded(px(6.)).overflow_hidden().bg(rgb(RAISED));
            let thumb = match &r.thumb {
                Some(p) => thumb.child(img(p.clone()).size_full()),
                None => thumb,
            };
            // Until yt-dlp reports the title, the "title" is the raw URL: keep it quiet.
            let title = div().text_size(px(14.)).line_height(px(20.)).truncate().child(r.title.clone());
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
                State::Failed(e) => (format!("Failed: {}", e.strip_prefix("ERROR: ").unwrap_or(e)), RED),
            };
            let meta = div().text_xs().text_color(rgb(color)).child(text);
            let meta = if matches!(r.state, State::Failed(_)) { meta.line_clamp(2) } else { meta.truncate() };
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
                .child(
                    div()
                        .max_w(px(300.))
                        .text_center()
                        .text_color(rgb(MUTED))
                        .child("Click Download under any YouTube video, or the ytgrab icon in Chrome’s toolbar."),
                )
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
        let running = self.rows.iter().filter(|r| matches!(r.state, State::Running(..))).count();
        let queued = self.rows.iter().filter(|r| matches!(r.state, State::Queued)).count();
        let summary =
            [(running > 0).then(|| format!("{running} downloading")), (queued > 0).then(|| format!("{queued} queued"))]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
        let any_done = self.rows.iter().any(|r| matches!(r.state, State::Done));
        let has_detail = !s.detail.is_empty();
        let open = self.details_open && has_detail;
        let status_bar = div()
            .flex_none()
            .border_t_1()
            .border_color(rgb(LINE))
            .when(open, |d| {
                d.child(div().px_4().pt(px(10.)).text_xs().text_color(rgb(WARN_TEXT)).child(s.detail.clone()))
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
                    .child(div().size(px(6.)).flex_none().rounded_full().bg(rgb(dot)))
                    .child(div().min_w_0().truncate().child(s.text.clone()))
                    .when(has_detail, |d| {
                        d.child(
                            ghost("details", if open { "Hide" } else { "Details" }, ring).h(px(24.)).px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.details_open = !this.details_open;
                                    cx.notify();
                                }),
                            ),
                        )
                    })
                    .child(div().flex_1())
                    .child(div().flex_none().child(summary))
                    .when(any_done, |d| {
                        d.child(ghost("clear-done", "Clear completed", ring).h(px(24.)).px_2().on_click(cx.listener(
                            |this, _, window, cx| {
                                window.focus(&this.focus);
                                this.rows.retain(|r| !matches!(r.state, State::Done));
                                cx.notify();
                            },
                        )))
                    }),
            );

        div()
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &FocusNext, window, cx| {
                window.focus_next();
                this.ring = true;
                cx.notify();
            }))
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
    let first_status = match tiny_http::Server::http(("127.0.0.1", PORT)) {
        Ok(server) => {
            let (ui, jobs) = (ui_tx.clone(), job_tx.clone());
            std::thread::spawn(move || serve(server, jobs, ui));
            let dir = dir.clone();
            std::thread::spawn(move || work(job_rx, dir, ui_tx));
            status(Kind::Busy, "Starting…", "")
        }
        Err(e) => {
            status(Kind::Warn, format!("Cannot listen on 127.0.0.1:{PORT}. Is ytgrab already open?"), e.to_string())
        }
    };

    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new("tab", FocusNext, None), KeyBinding::new("shift-tab", FocusPrev, None)]);
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
}
