//! OpenEOS: every gphoto2 camera setting in one window, plus live view, capture and a webcam feed.
use eframe::egui;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{Receiver, channel};

#[derive(Default, Debug, PartialEq)]
struct Setting {
    path: String,
    label: String,
    kind: String,
    readonly: bool,
    current: String,
    choices: Vec<String>,
}

/// Parses `gphoto2 --list-all-config`: a path line, then `Key: value` lines, then `END`.
fn parse(text: &str) -> Vec<Setting> {
    let mut out = Vec::new();
    let mut s = Setting::default();
    for line in text.lines() {
        if line.starts_with('/') {
            s = Setting { path: line.into(), ..Default::default() };
        } else if line == "END" {
            if !s.path.is_empty() {
                out.push(std::mem::take(&mut s));
            }
        } else if let Some((key, val)) = line.split_once(": ") {
            match key {
                "Label" => s.label = val.into(),
                "Type" => s.kind = val.into(),
                "Readonly" => s.readonly = val == "1",
                "Current" => s.current = val.into(),
                // "Choice: 3 1/250": drop the index, keep the value gphoto2 accepts back.
                "Choice" => s.choices.push(val.split_once(' ').map_or("", |(_, v)| v).into()),
                _ => {}
            }
        }
    }
    out
}

type Reply = Receiver<Result<String, String>>;

/// Runs `work` off the UI thread; the camera takes a second or more to answer.
fn spawn(ctx: &egui::Context, work: impl FnOnce() -> Result<String, String> + Send + 'static) -> Reply {
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        _ = tx.send(work());
        ctx.request_repaint();
    });
    rx
}

/// `ip` empty = USB auto-detect; otherwise the camera's address over Wi-Fi (PTP/IP).
fn gphoto2_command(ip: &str) -> Command {
    let mut c = Command::new("gphoto2");
    if !ip.is_empty() {
        c.arg("--port").arg(format!("ptpip:{ip}"));
    }
    c
}

fn gphoto2(ip: &str, args: &[&str]) -> Result<String, String> {
    match gphoto2_command(ip).args(args).output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).lines().find(|l| !l.trim().is_empty() && !l.contains("*** Error ***")).unwrap_or("gphoto2 failed").trim().into()),
        Err(e) => Err(format!("cannot run gphoto2 ({e}). Install: {}", install_hint())),
    }
}

/// The app drives the camera through the gphoto2 and ffmpeg programs; say how to get them.
fn install_hint() -> &'static str {
    match std::env::consts::OS {
        "macos" => "brew install gphoto2 ffmpeg",
        "windows" => "no native gphoto2 on Windows: use WSL2 with usbipd, then sudo apt install gphoto2 ffmpeg",
        _ => {
            let os = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
            let is = |id: &str| os.lines().any(|l| (l.starts_with("ID=") || l.starts_with("ID_LIKE=")) && l.contains(id));
            if is("arch") {
                "sudo pacman -S gphoto2 ffmpeg v4l2loopback-dkms"
            } else if is("debian") || is("ubuntu") {
                "sudo apt install gphoto2 ffmpeg v4l2loopback-dkms"
            } else if is("fedora") {
                "sudo dnf install gphoto2 ffmpeg (v4l2loopback from RPM Fusion)"
            } else {
                "gphoto2, ffmpeg and v4l2loopback from your package manager"
            }
        }
    }
}

const VIDEO_DEV: &str = "/dev/video10";
/// macOS has no v4l2loopback: the frames go out as MJPEG over UDP for an OBS Media Source,
/// whose Virtual Camera then acts as the webcam.
const MAC_FEED: &str = "udp://127.0.0.1:5600?pkt_size=65000";

/// Live view: `gphoto2 --capture-movie` streams concatenated JPEGs. The camera is busy while it
/// runs, so it is stopped (Ctrl-C style, or the 80D hangs) before any other gphoto2 call.
/// Every frame is also written to `sink` (the webcam's ffmpeg), which survives stream restarts.
struct Stream {
    child: Child,
    frames: Receiver<egui::ColorImage>,
    reader: std::thread::JoinHandle<()>,
}

impl Stream {
    fn start(ctx: &egui::Context, ip: &str, sink: Arc<Mutex<Option<ChildStdin>>>, show: Arc<AtomicBool>) -> Result<Self, String> {
        let mut child = gphoto2_command(ip)
            .args(["--stdout", "--capture-movie"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run gphoto2 ({e}). Install: {}", install_hint()))?;
        let mut out = child.stdout.take().unwrap();
        let (tx, frames) = channel();
        let ctx = ctx.clone();
        let reader = std::thread::spawn(move || {
            let (mut buf, mut chunk) = (Vec::new(), [0u8; 64 * 1024]);
            // A JPEG ends at FF D9 (FF is byte-stuffed inside the data), so split on that.
            while let Ok(n @ 1..) = out.read(&mut chunk) {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(end) = buf.windows(2).position(|w| w == [0xFF, 0xD9]) {
                    let frame: Vec<u8> = buf.drain(..end + 2).collect();
                    if let Some(w) = sink.lock().unwrap().as_mut() {
                        _ = w.write_all(&frame); // ffmpeg gone: the app notices via try_wait
                    }
                    if !show.load(Ordering::Relaxed) {
                        continue;
                    }
                    if let Ok(img) = image::load_from_memory_with_format(&frame, image::ImageFormat::Jpeg) {
                        let img = img.to_rgba8();
                        let size = [img.width() as usize, img.height() as usize];
                        _ = tx.send(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()));
                        ctx.request_repaint();
                    }
                }
            }
        });
        Ok(Stream { child, frames, reader })
    }

    /// The Child is only reaped here, so its PID still names our gphoto2 when it is signalled.
    /// If gphoto2 ignores the interrupt, it is killed after a few seconds so the app cannot hang.
    fn stop(mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            _ = Command::new("kill").args(["-INT", &self.child.id().to_string()]).status();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while matches!(self.child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ = self.child.kill();
        }
        _ = self.child.wait();
        _ = self.reader.join();
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tab {
    Favorites,
    Shooting,
    Image,
    Camera,
    Status,
    Advanced,
}

const TABS: [(Tab, &str); 6] = [
    (Tab::Favorites, "★ Favorites"),
    (Tab::Shooting, "Shooting"),
    (Tab::Image, "Image"),
    (Tab::Camera, "Camera"),
    (Tab::Status, "Status"),
    (Tab::Advanced, "Advanced"),
];

/// Tab by gphoto2 path group; a few settings that live under /main/settings are developer-level.
fn tab_of(path: &str) -> Tab {
    const ADVANCED: [&str; 3] = ["evfmode", "customfuncex", "testolc"];
    if ADVANCED.contains(&path.rsplit('/').next().unwrap_or("")) {
        return Tab::Advanced;
    }
    match path.split('/').nth(2) {
        Some("capturesettings") => Tab::Shooting,
        Some("imgsettings") => Tab::Image,
        Some("settings") => Tab::Camera,
        Some("status") => Tab::Status,
        _ => Tab::Advanced,
    }
}

const SEED_FAVORITES: [&str; 7] = [
    "/main/imgsettings/iso",
    "/main/capturesettings/shutterspeed",
    "/main/capturesettings/aperture",
    "/main/capturesettings/exposurecompensation",
    "/main/imgsettings/whitebalance",
    "/main/capturesettings/picturestyle",
    "/main/capturesettings/focusmode",
];

fn config_file(name: &str) -> Option<PathBuf> {
    // XDG spec: an empty or relative XDG_CONFIG_HOME is ignored.
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    let base = xdg.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let dir = base.join("openeos");
    // Earlier versions were called canon-settings: move their favorites and Wi-Fi address over once.
    let old = base.join("canon-settings");
    if !dir.exists() && old.is_dir() {
        _ = std::fs::rename(&old, &dir);
    }
    Some(dir.join(name))
}

fn favorites_file() -> Option<PathBuf> {
    config_file("favorites")
}

fn load_ip() -> String {
    config_file("wifi-ip").and_then(|p| std::fs::read_to_string(p).ok()).map(|t| t.trim().to_string()).unwrap_or_default()
}

fn save_ip(ip: &str) -> Result<(), String> {
    let path = config_file("wifi-ip").ok_or("no HOME, address not saved")?;
    std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| std::fs::write(&path, ip)).map_err(|e| format!("cannot save address: {e}"))
}

fn parse_favorites(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// Missing file means first run (seeds); an empty file means the user cleared them all.
fn load_favorites() -> (Vec<String>, Option<String>) {
    let seeds = || SEED_FAVORITES.map(String::from).to_vec();
    match favorites_file().map(std::fs::read_to_string) {
        Some(Ok(text)) => (parse_favorites(&text), None),
        Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound => (seeds(), Some(format!("cannot read favorites: {e}"))),
        _ => (seeds(), None),
    }
}

fn save_favorites(favs: &[String]) -> Result<(), String> {
    let path = favorites_file().ok_or("no HOME, favorites not saved")?;
    std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| std::fs::write(&path, favs.join("\n") + "\n")).map_err(|e| format!("cannot save favorites: {e}"))
}

enum Job {
    Load,
    Set(usize, String),
    Capture,
}

struct App {
    settings: Vec<Setting>,
    edits: Vec<String>,
    filter: String,
    status: String,
    /// ffmpeg turning our frames into the /dev/video10 webcam; `sink` is its stdin.
    webcam_on: bool,
    sink: Arc<Mutex<Option<ChildStdin>>>,
    sink_child: Option<Child>,
    job: Option<(Job, Reply)>,
    /// User wants the picture in the window. The stream runs for it or the webcam, paused during jobs.
    want_preview: bool,
    show: Arc<AtomicBool>,
    preview: Option<Stream>,
    texture: Option<egui::TextureHandle>,
    tab: Tab,
    favorites: Vec<String>,
    /// Camera address for Wi-Fi (PTP/IP); empty means USB. `ip_edit` is the text box.
    ip: String,
    ip_edit: String,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let (favorites, fav_error) = load_favorites();
        let mut app = App {
            settings: vec![], edits: vec![], filter: String::new(), status: String::new(),
            webcam_on: false, sink: Arc::default(), sink_child: None, job: None,
            want_preview: false, show: Arc::default(), preview: None, texture: None,
            tab: Tab::Favorites, favorites, ip: load_ip(), ip_edit: String::new(),
        };
        app.ip_edit = app.ip.clone();
        app.load(ctx);
        if let Some(e) = fav_error {
            app.status = e;
        }
        app
    }

    /// Switches between USB (blank) and Wi-Fi: ends the stream so it restarts on the new link, then reloads.
    fn set_ip(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        let ip = self.ip_edit.trim().to_string();
        if ip.starts_with('-') || ip.contains(char::is_whitespace) {
            self.status = "Not a valid address".into();
            return;
        }
        if let Some(p) = self.preview.take() {
            p.stop();
        }
        self.texture = None;
        self.settings.clear();
        self.ip = ip;
        if let Err(e) = save_ip(&self.ip) {
            self.status = e;
        }
        self.load(ctx);
    }

    fn load(&mut self, ctx: &egui::Context) {
        self.status = "Reading camera…".into();
        let (stream, ip) = (self.preview.take(), self.ip.clone());
        self.job = Some((Job::Load, spawn(ctx, move || {
            if let Some(p) = stream {
                p.stop();
            }
            gphoto2(&ip, &["--list-all-config"])
        })));
    }

    fn set(&mut self, ctx: &egui::Context, i: usize, value: String) {
        self.status = format!("Setting {}…", self.settings[i].label);
        let arg = format!("{}={value}", self.settings[i].path);
        let (stream, ip) = (self.preview.take(), self.ip.clone());
        self.job = Some((Job::Set(i, value), spawn(ctx, move || {
            if let Some(p) = stream {
                p.stop();
            }
            gphoto2(&ip, &["--set-config-value", &arg])
        })));
    }

    /// Takes one photo and downloads it as the camera stores it (RAW if Image Format is RAW).
    /// Kept on the card too. Live view is ended by the preview stop and resumes after the job.
    fn capture(&mut self, ctx: &egui::Context) {
        let Some(home) = std::env::var_os("HOME") else {
            self.status = "no HOME, cannot choose where to save".into();
            return;
        };
        let dir = PathBuf::from(home).join("Pictures/OpenEOS");
        self.status = "Capturing…".into();
        let (stream, ip) = (self.preview.take(), self.ip.clone());
        self.job = Some((Job::Capture, spawn(ctx, move || {
            if let Some(p) = stream {
                p.stop();
            }
            std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            let pattern = dir.join("%Y%m%d-%H%M%S.%C");
            let out = gphoto2(&ip, &["--capture-image-and-download", "--keep", "--filename", &pattern.to_string_lossy()])?;
            let saved: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("Saving file as ")).collect();
            if saved.is_empty() {
                return Err("capture gave no file".into());
            }
            let list = saved.join(", ");
            let jpeg_only = saved.iter().all(|f| f.to_lowercase().ends_with(".jpg"));
            Ok(if jpeg_only { format!("{list} (JPEG only: set Image Format to RAW in the Image tab)") } else { list })
        })));
    }

    fn start_webcam(&mut self) -> Result<(), String> {
        let output: Vec<&str> = match std::env::consts::OS {
            "linux" if !std::path::Path::new(VIDEO_DEV).exists() => {
                return Err(format!("{VIDEO_DEV} missing: sudo modprobe v4l2loopback exclusive_caps=1 card_label=\"OpenEOS\" video_nr=10 (or install: {})", install_hint()));
            }
            "linux" => vec!["-vf", "format=yuv420p", "-f", "v4l2", VIDEO_DEV],
            "macos" => vec!["-fps_mode", "vfr", "-q:v", "5", "-f", "mjpeg", MAC_FEED],
            _ => return Err("Webcam output works on Linux and macOS; live view and capture work wherever gphoto2 does".into()),
        };
        let mut child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-use_wallclock_as_timestamps", "1", "-f", "mjpeg", "-fflags", "nobuffer", "-flags", "low_delay", "-threads", "1", "-i", "-"])
            .args(output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run ffmpeg ({e}). Install: {}", install_hint()))?;
        *self.sink.lock().unwrap() = child.stdin.take();
        self.sink_child = Some(child);
        self.webcam_on = true;
        Ok(())
    }

    fn stop_webcam(&mut self) {
        self.sink.lock().unwrap().take(); // closing stdin ends ffmpeg
        if let Some(mut c) = self.sink_child.take() {
            _ = c.wait();
        }
        self.webcam_on = false;
    }

    fn toggle_webcam(&mut self, ctx: &egui::Context) {
        if self.webcam_on {
            self.stop_webcam();
            self.status = "Webcam off".into();
        } else {
            self.status = match self.start_webcam() {
                Ok(()) if cfg!(target_os = "macos") => "Webcam on: in OBS add a Media Source (udp://127.0.0.1:5600, format mjpeg, no buffering), then Start Virtual Camera".into(),
                Ok(()) => "Webcam on".into(),
                Err(e) => e,
            };
        }
        self.sync_stream(ctx);
    }

    fn toggle_preview(&mut self, ctx: &egui::Context) {
        self.want_preview = !self.want_preview;
        if !self.want_preview {
            self.texture = None;
            if let Some(p) = self.preview.take() {
                p.stop();
            }
        }
        self.sync_stream(ctx);
    }

    /// Runs the camera stream iff the window or the webcam wants it and no job is using the camera.
    fn sync_stream(&mut self, ctx: &egui::Context) {
        self.show.store(self.want_preview, Ordering::Relaxed);
        if self.sink_child.as_mut().is_some_and(|c| c.try_wait().is_ok_and(|s| s.is_some())) {
            self.stop_webcam();
            self.status = format!("Webcam output stopped (is another program writing to {VIDEO_DEV}?)");
        }
        let wanted = self.want_preview || self.webcam_on;
        if let Some(p) = &self.preview {
            // Reader thread ends when gphoto2 exits (camera gone, error).
            if p.reader.is_finished() {
                if let Some(p) = self.preview.take() {
                    p.stop();
                }
                self.want_preview = false;
                self.texture = None;
                self.stop_webcam();
                self.status = "Camera stream stopped (busy or unplugged?)".into();
            } else if !wanted && let Some(p) = self.preview.take() {
                p.stop();
            }
        } else if wanted && self.job.is_none() {
            match Stream::start(ctx, &self.ip, self.sink.clone(), self.show.clone()) {
                Ok(p) => (self.preview, self.status) = (Some(p), String::new()),
                Err(e) => {
                    self.want_preview = false;
                    self.stop_webcam();
                    self.status = e;
                }
            }
        }
        if let Some(p) = &self.preview {
            // Drain always; frames queued before Live view was turned off must not bring the panel back.
            if let Some(img) = p.frames.try_iter().last().filter(|_| self.want_preview) {
                match &mut self.texture {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.texture = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
                }
            }
        }
    }

    fn finish_job(&mut self) {
        let Some((_, rx)) = &self.job else { return };
        let Ok(result) = rx.try_recv() else { return };
        let (job, _) = self.job.take().unwrap();
        match (job, result) {
            (Job::Load, Ok(text)) => {
                self.settings = parse(&text);
                self.edits = self.settings.iter().map(|s| s.current.clone()).collect();
                self.status = format!("{} settings", self.settings.len());
            }
            (Job::Set(i, value), Ok(_)) => {
                self.status = format!("{} = {value}", self.settings[i].label);
                self.settings[i].current = value;
            }
            (Job::Set(i, _), Err(e)) => {
                self.edits[i] = self.settings[i].current.clone();
                self.status = e;
            }
            (Job::Load, Err(e)) => self.status = e,
            (Job::Capture, Ok(saved)) => self.status = format!("Saved {saved}"),
            (Job::Capture, Err(e)) => self.status = e,
        }
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            let label = if self.webcam_on { "Webcam: ON" } else { "Webcam: OFF" };
            if ui.add_enabled(self.job.is_none(), egui::Button::new(label).selected(self.webcam_on)).clicked() {
                self.toggle_webcam(ctx);
            }
            let can_preview = self.job.is_none() || self.want_preview;
            if ui.add_enabled(can_preview, egui::Button::new("Live view").selected(self.want_preview)).clicked() {
                self.toggle_preview(ctx);
            }
            if ui.add_enabled(self.job.is_none(), egui::Button::new("Capture")).clicked() {
                self.capture(ctx);
            }
            if ui.add_enabled(self.job.is_none(), egui::Button::new("Reload")).clicked() {
                self.load(ctx);
            }
            let r = ui.add_enabled(self.job.is_none(), egui::TextEdit::singleline(&mut self.ip_edit).hint_text("Wi-Fi camera IP (blank = USB)").desired_width(190.0));
            if r.lost_focus() && self.ip_edit.trim() != self.ip {
                self.set_ip(ctx);
            }
            if self.job.is_some() {
                ui.spinner();
            }
            ui.label(&self.status);
        });
    }

    fn preview_ui(&self, ui: &mut egui::Ui) {
        ui.centered_and_justified(|ui| match &self.texture {
            Some(t) => _ = ui.add(egui::Image::new(t).shrink_to_fit()),
            None => _ = ui.spinner(),
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let searching = !self.filter.is_empty();
        ui.horizontal_wrapped(|ui| {
            for (t, name) in TABS {
                if ui.selectable_label(!searching && self.tab == t, name).clicked() {
                    self.tab = t;
                    self.filter.clear();
                }
            }
        });
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Search all settings…").desired_width(f32::INFINITY));
        ui.separator();
        let filter = self.filter.to_lowercase();
        let shown = |s: &Setting| s.kind != "SECTION" && !s.path.starts_with("/main/actions/");
        let rows: Vec<usize> = if searching {
            (0..self.settings.len()).filter(|&i| shown(&self.settings[i]) && format!("{} {}", self.settings[i].label, self.settings[i].path).to_lowercase().contains(&filter)).collect()
        } else if self.tab == Tab::Favorites {
            self.favorites.iter().filter_map(|p| self.settings.iter().position(|s| s.path == *p)).collect()
        } else {
            (0..self.settings.len()).filter(|&i| shown(&self.settings[i]) && tab_of(&self.settings[i].path) == self.tab).collect()
        };
        let (mut change, mut star) = (None, None);
        ui.add_enabled_ui(self.job.is_none(), |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                if rows.is_empty() {
                    ui.weak(match (self.settings.is_empty(), searching, self.tab) {
                        (true, ..) => "No settings loaded (click Reload)",
                        (_, true, _) => "No match",
                        (_, _, Tab::Favorites) => "No favorites yet: click ☆ on any setting",
                        _ => "Nothing here",
                    });
                }
                egui::Grid::new("settings").num_columns(3).striped(true).show(ui, |ui| {
                    for &i in &rows {
                        let s = &self.settings[i];
                        let fav = self.favorites.contains(&s.path);
                        if ui.add(egui::Button::new(if fav { "★" } else { "☆" }).frame(false)).clicked() {
                            star = Some(s.path.clone());
                        }
                        ui.horizontal(|ui| {
                            ui.label(&s.label).on_hover_text(&s.path);
                            if searching {
                                ui.weak(TABS.iter().find(|(t, _)| *t == tab_of(&s.path)).map_or("", |(_, n)| n.trim_start_matches("★ ")));
                            }
                        });
                        let edit = &mut self.edits[i];
                        if s.readonly || s.kind == "DATE" {
                            ui.label(&s.current);
                        } else if !s.choices.is_empty() {
                            egui::ComboBox::from_id_salt(&s.path).selected_text(&s.current).show_ui(ui, |ui| {
                                for c in &s.choices {
                                    if ui.selectable_label(*c == s.current, c).clicked() && *c != s.current {
                                        change = Some((i, c.clone()));
                                    }
                                }
                            });
                        } else if s.kind == "TOGGLE" {
                            let mut on = s.current == "1";
                            if ui.checkbox(&mut on, "").changed() {
                                change = Some((i, if on { "1" } else { "0" }.into()));
                            }
                        } else {
                            let r = ui.add(egui::TextEdit::singleline(edit).id_salt(&s.path));
                            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && *edit != s.current {
                                change = Some((i, edit.clone()));
                            }
                        }
                        ui.end_row();
                    }
                });
                if self.tab == Tab::Favorites && !searching && !self.settings.is_empty() {
                    let missing: Vec<&str> = self.favorites.iter().filter(|p| !self.settings.iter().any(|s| s.path == **p)).map(String::as_str).collect();
                    if !missing.is_empty() {
                        ui.weak(format!("Not on camera: {}", missing.join(", ")));
                    }
                }
            });
        });
        if let Some(path) = star {
            match self.favorites.iter().position(|p| *p == path) {
                Some(k) => _ = self.favorites.remove(k),
                None => self.favorites.push(path),
            }
            if let Err(e) = save_favorites(&self.favorites) {
                self.status = e;
            }
        }
        if let Some((i, value)) = change {
            self.set(ctx, i, value);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.finish_job();
        let ctx = ui.ctx().clone();
        self.sync_stream(&ctx);
        egui::Panel::top("bar").show(ui, |ui| self.top_bar(ui, &ctx));
        let wide = ui.available_width() > ui.available_height() * 1.2;
        if !(self.want_preview || self.texture.is_some()) {
            egui::CentralPanel::default().show(ui, |ui| self.settings_ui(ui, &ctx));
        } else if wide {
            egui::Panel::right("settings").default_size(480.0).size_range(320.0..=900.0).show(ui, |ui| self.settings_ui(ui, &ctx));
            egui::CentralPanel::default().show(ui, |ui| self.preview_ui(ui));
        } else {
            let h = ui.available_height();
            egui::Panel::top("preview").resizable(true).default_size(h * 0.45).size_range(120.0..=h * 0.75).show(ui, |ui| self.preview_ui(ui));
            egui::CentralPanel::default().show(ui, |ui| self.settings_ui(ui, &ctx));
        }
    }

    /// Ends gphoto2 and ffmpeg so the camera and the video device are free for other programs.
    fn on_exit(&mut self) {
        if let Some(p) = self.preview.take() {
            p.stop();
        }
        self.stop_webcam();
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "openeos",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_title("OpenEOS").with_app_id("openeos").with_inner_size([560.0, 720.0]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))),
    )
}

#[test]
fn parses_list_all_config() {
    let text = "/main/imgsettings/iso\nLabel: ISO Speed\nReadonly: 0\nType: RADIO\nCurrent: 400\nChoice: 0 Auto\nChoice: 1 100\nEND\n/main/status/lensname\nLabel: Lens Name\nReadonly: 1\nType: TEXT\nCurrent: EF50mm f/1.8 STM\nEND\n";
    let s = parse(text);
    assert_eq!(s.len(), 2);
    assert_eq!((s[0].label.as_str(), s[0].current.as_str()), ("ISO Speed", "400"));
    assert_eq!(s[0].choices, ["Auto", "100"]);
    assert!(s[1].readonly && s[1].choices.is_empty());
    assert_eq!(s[1].current, "EF50mm f/1.8 STM");
}

#[test]
fn tabs_and_favorites() {
    assert_eq!(tab_of("/main/imgsettings/iso"), Tab::Image);
    assert_eq!(tab_of("/main/capturesettings/shutterspeed"), Tab::Shooting);
    assert_eq!(tab_of("/main/settings/evfmode"), Tab::Advanced);
    assert_eq!(tab_of("/main/status/lensname"), Tab::Status);
    assert_eq!(tab_of("/main/other/d101"), Tab::Advanced);
    assert_eq!(parse_favorites(" /a \n\n/b\n"), ["/a", "/b"]);
}

#[test]
fn install_hint_names_gphoto2() {
    assert!(install_hint().contains("gphoto2"));
}
