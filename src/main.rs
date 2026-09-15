mod theme;
mod zoom;

use eframe::egui;
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

const JSON_PATH: &str =
    "/SNS/VENUS/shared/software/menu/list_marimo_general_users_applications.json";
/// Imaging instruments the portal can provision into:
/// (display name, IPTS root, header logo). MARS is CG-1D at HFIR; it has no
/// instrument-specific logo, so it uses the generic ORNL Neutron Imaging one.
/// VENUS (index 0) is the default.
const INSTRUMENTS: &[(&str, &str, &str)] = &[
    (
        "VENUS",
        "/SNS/VENUS",
        "/SNS/VENUS/shared/software/logos/logo_with_green_neutron_rays.png",
    ),
    (
        "MARS",
        "/HFIR/CG1D",
        "/SNS/VENUS/shared/software/logos/ImagingLogo.png",
    ),
];
// Directories that must never be copied into the user's IPTS folder.
const SKIP_DIRS: &[&str] = &["__pycache__", "__marimo__"];
/// Maintenance script that lists and kills stuck browser sessions (source in
/// `scripts/` of this repo; deployed next to the portal binary).
/// Also run in `list` mode when the firefox we spawn fails: the profile lives
/// on shared NFS/GPFS storage, so a firefox running on ANY analysis machine
/// locks it — the scan shows WHERE. Both modes work over passwordless SSH,
/// which the script sets up itself for users who have no SSH key yet.
const FIX_BROWSER_SCRIPT: &str = "/SNS/VENUS/shared/software/bin/list_and_fix_running_browser.sh";

/// The scan report lists offending hosts as "[host] N process(es):" blocks.
fn scan_found_processes(report: &str) -> bool {
    report
        .lines()
        .any(|l| l.trim_start().starts_with('[') && l.contains("process(es):"))
}

/// Instant "where is my Firefox running" check: Firefox writes a `lock`
/// symlink inside each profile pointing to "ip:+pid" of the owning process,
/// so when the shared profile is locked by a session on ANOTHER machine the
/// symlink names that machine directly — no SSH scan needed. Returns a
/// report for the pop-up window, or None when no remote lock is held (a
/// lock held by this machine is harmless: firefox just opens a new tab).
fn firefox_remote_lock_report() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let profiles = Path::new(&home).join(".mozilla/firefox");
    let local_ips: Vec<String> = Command::new("hostname")
        .arg("-I")
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut blocks = Vec::new();
    for entry in fs::read_dir(&profiles).ok()?.flatten() {
        let Ok(target) = fs::read_link(entry.path().join("lock")) else {
            continue;
        };
        let target = target.to_string_lossy().into_owned();
        let Some((ip, pid)) = target.split_once(":+") else {
            continue;
        };
        if ip.starts_with("127.") || local_ips.iter().any(|l| l == ip) {
            continue;
        }
        let host = Command::new("getent")
            .args(["hosts", ip])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .nth(1)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| ip.to_owned());
        blocks.push(format!(
            "[{host}] ({ip}) firefox PID {pid} holds the profile lock\n    \
             (profile {})\n    to close it from here:  ssh {host}  then  kill {pid}",
            entry.file_name().to_string_lossy(),
        ));
    }
    if blocks.is_empty() {
        None
    } else {
        blocks.push(
            "If Firefox is NOT actually running there, the lock is stale:\n    \
             delete the 'lock' and '.parentlock' files in that profile folder\n    \
             under ~/.mozilla/firefox/."
                .to_owned(),
        );
        Some(blocks.join("\n\n"))
    }
}

struct AppEntry {
    description: String,
    path: String,
    marimo_path: String,
    screenshot: String,
    /// Optional section name ("category" key in the JSON). Entries sharing a
    /// category are grouped under a collapsible section in the list.
    category: String,
    /// Instruments the notebook applies to ("instruments" key in the JSON,
    /// a list of `INSTRUMENTS` names). Empty = available everywhere. The
    /// entry is shown disabled when the selected instrument is not listed.
    instruments: Vec<String>,
}

impl AppEntry {
    fn available_at(&self, instrument: &str) -> bool {
        self.instruments.is_empty()
            || self.instruments.iter().any(|i| i.eq_ignore_ascii_case(instrument))
    }
}

/// One row of the application list: either a standalone app or a named
/// section containing several apps. Indices point into `MyApp::applications`.
enum DisplayItem {
    App(usize),
    Section(String, Vec<usize>),
}

/// An IPTS the current user can see under the instrument root.
struct IptsEntry {
    label: String,
    path: PathBuf,
    writable: bool,
}

/// A static logo image loaded into a texture, plus its aspect ratio for sizing.
struct Logo {
    texture: egui::TextureHandle,
    aspect: f32, // width / height
}

impl Logo {
    /// Load the image at `path` into a GPU texture. Returns `None` if the file
    /// is missing or cannot be decoded.
    fn load(ctx: &egui::Context, path: &str) -> Option<Self> {
        let img = image::open(path).ok()?.to_rgba8();
        let (w, h) = (img.width(), img.height());
        let color_image =
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
        let texture = ctx.load_texture("logo", color_image, egui::TextureOptions::LINEAR);
        let aspect = if h > 0 { w as f32 / h as f32 } else { 1.0 };
        Some(Self { texture, aspect })
    }
}

fn load_applications() -> Vec<(String, AppEntry)> {
    let content = match fs::read_to_string(JSON_PATH) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to read {}: {}", JSON_PATH, e);
            return Vec::new();
        }
    };

    // The file uses Python-style single quotes; replace with double quotes.
    let content = content.replace('\'', "\"");

    let map: BTreeMap<String, Value> = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to parse JSON: {}", e);
            return Vec::new();
        }
    };

    map.into_iter()
        .map(|(name, val)| {
            let description = val
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let path = val
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let marimo_path = val
                .get("marimo_path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let screenshot = val
                .get("screenshot")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let category = val
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let instruments = val
                .get("instruments")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            (
                name,
                AppEntry {
                    description,
                    path,
                    marimo_path,
                    screenshot,
                    category,
                    instruments,
                },
            )
        })
        .collect()
}

/// Group the flat application list for display: uncategorized apps stay
/// standalone rows, categorized apps collapse under one section per category,
/// and everything (apps and sections alike) sorts alphabetically.
fn build_display(applications: &[(String, AppEntry)]) -> Vec<DisplayItem> {
    let mut sections: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut rows: Vec<(String, DisplayItem)> = Vec::new();
    for (i, (name, entry)) in applications.iter().enumerate() {
        if entry.category.is_empty() {
            rows.push((name.clone(), DisplayItem::App(i)));
        } else {
            sections.entry(entry.category.clone()).or_default().push(i);
        }
    }
    for (category, indices) in sections {
        rows.push((category.clone(), DisplayItem::Section(category, indices)));
    }
    rows.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    rows.into_iter().map(|(_, item)| item).collect()
}

fn can_access(path: &Path) -> bool {
    let Ok(cstr) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(cstr.as_ptr(), libc::R_OK | libc::X_OK) == 0 }
}

fn can_write(path: &Path) -> bool {
    let Ok(cstr) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(cstr.as_ptr(), libc::W_OK) == 0 }
}

fn user_id() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".to_string())
}

/// List the IPTS-* directories the user can read under `root`, sorted by number.
/// An entry is `writable` when its `shared/` folder accepts writes (we provision
/// into `shared/notebooks/marimo/`).
fn list_ipts(root: &Path) -> Result<Vec<IptsEntry>, String> {
    let dir = fs::read_dir(root).map_err(|e| format!("Cannot read {}: {e}", root.display()))?;
    let mut ipts: Vec<(u64, IptsEntry)> = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(suffix) = name_str.strip_prefix("IPTS-") else {
            continue;
        };
        let path = entry.path();
        if !can_access(&path) {
            continue;
        }
        let writable = can_write(&path.join("shared"));
        let num: u64 = suffix.parse().unwrap_or(u64::MAX);
        ipts.push((
            num,
            IptsEntry {
                label: name_str.into_owned(),
                path,
                writable,
            },
        ));
    }
    ipts.sort_by_key(|(n, _)| *n);
    Ok(ipts.into_iter().map(|(_, e)| e).collect())
}

/// `<instrument root>/<ipts>/shared/notebooks/imaging_marimo_<user>`
fn destination_for(ipts: &IptsEntry) -> PathBuf {
    ipts.path
        .join("shared")
        .join("notebooks")
        .join(format!("imaging_marimo_{}", user_id()))
}

/// Copy a directory tree, skipping cache folders.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let name = entry.file_name();
        if SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            if dst_path.exists() {
                let _ = fs::remove_file(&dst_path);
            }
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Recursively set rwx-for-all on a path and everything inside it.
/// Entries owned by other users cannot be chmod'ed and are left as-is.
fn open_permissions_recursive(path: &Path) {
    let open_perms = fs::Permissions::from_mode(0o777);
    let _ = fs::set_permissions(path, open_perms);
    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                open_permissions_recursive(&entry.path());
            }
        }
    }
}

/// Create the destination folder (if missing) and copy the selected notebook plus
/// its sibling `utilities/` package into it. Returns the notebook file name to run.
fn provision(app: &AppEntry, dest: &Path) -> Result<PathBuf, String> {
    let notebook = Path::new(&app.path);
    let file_name = notebook
        .file_name()
        .ok_or_else(|| format!("Invalid notebook path: {}", app.path))?
        .to_owned();
    let src_dir = notebook
        .parent()
        .ok_or_else(|| format!("Cannot determine source folder for {}", app.path))?;

    fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    // Everybody must be able to read/write/traverse the notebooks folders.
    // The parent `notebooks` dir may pre-exist and belong to another user,
    // in which case chmod fails and we leave it as-is.
    let open_perms = fs::Permissions::from_mode(0o777);
    if let Some(notebooks_dir) = dest.parent() {
        let _ = fs::set_permissions(notebooks_dir, open_perms.clone());
    }
    fs::set_permissions(dest, open_perms)
        .map_err(|e| format!("chmod 777 {}: {e}", dest.display()))?;

    // Copy the notebook itself.
    let dst_notebook = dest.join(&file_name);
    if dst_notebook.exists() {
        let _ = fs::remove_file(&dst_notebook);
    }
    fs::copy(notebook, &dst_notebook)
        .map_err(|e| format!("copy {} -> {}: {e}", notebook.display(), dst_notebook.display()))?;

    // Copy the sibling `utilities/` package (relative import dependency), if present.
    let utilities_src = src_dir.join("utilities");
    if utilities_src.is_dir() {
        let utilities_dst = dest.join("utilities");
        copy_dir_recursive(&utilities_src, &utilities_dst).map_err(|e| {
            format!(
                "copy {} -> {}: {e}",
                utilities_src.display(),
                utilities_dst.display()
            )
        })?;
    }

    // Whether the destination pre-existed or was just provisioned, make sure
    // everything in it (notebooks included) is read/write/exec by everybody.
    open_permissions_recursive(dest);

    Ok(PathBuf::from(file_name))
}

fn main() -> eframe::Result {
    let mut options = eframe::NativeOptions::default();
    options.viewport = options.viewport.with_inner_size(egui::vec2(880.0, 780.0));
    eframe::run_native(
        &format!("{} General Tools", INSTRUMENTS[0].0),
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_theme(theme::load());
            theme::apply(&cc.egui_ctx);
            cc.egui_ctx.set_zoom_factor(zoom::load());
            Ok(Box::new(MyApp::new()))
        }),
    )
}

struct MyApp {
    applications: Vec<(String, AppEntry)>,
    /// Grouped view of `applications` (sections + standalone rows).
    app_display: Vec<DisplayItem>,
    selected: Option<usize>,
    /// Index into `INSTRUMENTS` of the currently selected instrument.
    instrument: usize,
    ipts_entries: Vec<IptsEntry>,
    ipts_error: Option<String>,
    ipts_selected: Option<usize>,
    manual_ipts: String,
    manual_ipts_msg: Option<(String, egui::Color32)>,
    scroll_to_ipts: bool,
    launch_time: Option<Instant>,
    launch_status: Option<(String, egui::Color32)>,
    screenshot_texture: Option<egui::TextureHandle>,
    logo: Option<Logo>,
    /// Instrument whose logo is currently loaded (`None` = not loaded yet).
    logo_instrument: Option<usize>,
    /// Message shown next to the browser-cleanup button.
    cleanup_msg: Option<(String, egui::Color32)>,
    /// Delivers the cleanup script's report once it exits.
    cleanup_rx: Option<mpsc::Receiver<String>>,
    /// The output-reader threads report a failed firefox spawn/exit here;
    /// receiving triggers the cross-machine browser scan.
    browser_fail_tx: mpsc::Sender<String>,
    browser_fail_rx: mpsc::Receiver<String>,
    /// Report channel of a running `FIX_BROWSER_SCRIPT list` scan.
    scan_rx: Option<mpsc::Receiver<String>>,
    /// The finished scan's report, shown in the pop-up window.
    scan_output: Option<String>,
    scan_window_open: bool,
    /// Whether the pop-up shows a "browser found elsewhere" scan or the
    /// result of the "Fix browser issue" kill run (different heading).
    scan_window_kind: ScanWindowKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanWindowKind {
    /// Firefox failed to open: where is it already running?
    Scan,
    /// The user clicked "Fix browser issue": what did the kill run do?
    Kill,
}

/// Summary of a `FIX_BROWSER_SCRIPT kill` report, from its closing lines
/// ("Found N process(es) on M host(s); K still running after kill." and
/// "A host(s) clean, B unreachable ...").
struct KillSummary {
    found: usize,
    hosts: usize,
    remaining: usize,
    clean: usize,
    unreachable: usize,
    auth_failed: bool,
}

fn parse_kill_summary(report: &str) -> KillSummary {
    // Pull every integer out of a line, in order.
    fn ints(line: &str) -> Vec<usize> {
        line.split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    }
    let mut s = KillSummary {
        found: 0,
        hosts: 0,
        remaining: 0,
        clean: 0,
        unreachable: 0,
        auth_failed: report.contains("Permission denied"),
    };
    for line in report.lines() {
        let line = line.trim();
        if line.starts_with("Found ") && line.contains("still running after kill") {
            let n = ints(line);
            s.found = n.first().copied().unwrap_or(0);
            s.hosts = n.get(1).copied().unwrap_or(0);
            s.remaining = n.get(2).copied().unwrap_or(0);
        } else if line.contains("host(s) clean") && line.contains("unreachable") {
            let n = ints(line);
            s.clean = n.first().copied().unwrap_or(0);
            s.unreachable = n.get(1).copied().unwrap_or(0);
        }
    }
    s
}

impl MyApp {
    fn new() -> Self {
        let applications = load_applications();
        let app_display = build_display(&applications);
        let (browser_fail_tx, browser_fail_rx) = mpsc::channel();
        let mut app = Self {
            applications,
            app_display,
            selected: None,
            instrument: 0, // VENUS by default
            ipts_entries: Vec::new(),
            ipts_error: None,
            ipts_selected: None,
            manual_ipts: String::new(),
            manual_ipts_msg: None,
            scroll_to_ipts: false,
            launch_time: None,
            launch_status: None,
            screenshot_texture: None,
            logo: None,
            logo_instrument: None,
            cleanup_msg: None,
            cleanup_rx: None,
            browser_fail_tx,
            browser_fail_rx,
            scan_rx: None,
            scan_output: None,
            scan_window_open: false,
            scan_window_kind: ScanWindowKind::Scan,
        };
        app.reload_ipts();
        app
    }

    /// Rebuild the IPTS list from the selected instrument's root and clear any
    /// selection/state tied to the previous instrument.
    fn reload_ipts(&mut self) {
        // Drop a selected application that the new instrument cannot use.
        if let Some(idx) = self.selected {
            if !self.app_available(idx) {
                self.selected = None;
                self.screenshot_texture = None;
            }
        }
        let root = Path::new(INSTRUMENTS[self.instrument].1);
        let (ipts_entries, ipts_error) = match list_ipts(root) {
            Ok(list) => (list, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        self.ipts_entries = ipts_entries;
        self.ipts_error = ipts_error;
        self.ipts_selected = None;
        self.manual_ipts.clear();
        self.manual_ipts_msg = None;
        self.launch_status = None;
    }

    /// Whether application `idx` applies to the selected instrument.
    fn app_available(&self, idx: usize) -> bool {
        self.applications[idx]
            .1
            .available_at(INSTRUMENTS[self.instrument].0)
    }

    fn load_screenshot(&mut self, ctx: &egui::Context, path: &str) {
        self.screenshot_texture = None;
        if path.is_empty() {
            return;
        }
        let img = match image::open(path) {
            Ok(img) => img.to_rgba8(),
            Err(e) => {
                eprintln!("Failed to load screenshot {}: {}", path, e);
                return;
            }
        };
        let size = [img.width() as usize, img.height() as usize];
        let pixels = img.into_raw();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
        self.screenshot_texture =
            Some(ctx.load_texture("screenshot", color_image, egui::TextureOptions::LINEAR));
    }

    /// Provision the destination folder and launch the selected notebook with marimo.
    fn launch(&mut self) {
        let (Some(ai), Some(ii)) = (self.selected, self.ipts_selected) else {
            return;
        };
        let dest = destination_for(&self.ipts_entries[ii]);
        let app = &self.applications[ai].1;

        let notebook_name = match provision(app, &dest) {
            Ok(name) => name,
            Err(e) => {
                self.launch_status = Some((e, theme::DANGER));
                return;
            }
        };

        let marimo_bin = app.marimo_path.clone();
        println!(
            "Launching: {} run {} (cwd {})",
            marimo_bin,
            notebook_name.display(),
            dest.display()
        );
        match Command::new(&marimo_bin)
            .arg("run")
            .arg(&notebook_name)
            .arg("--headless")
            // Provisioned IPTS folders have no pyproject.toml/.marimo.toml in
            // their ancestry, so marimo would fall back to its 8 MB default;
            // the branding cell alone is ~8.5 MB and trips the truncation
            // banner at startup.
            .env("MARIMO_OUTPUT_MAX_BYTES", "20000000")
            .current_dir(&dest)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                for stream in [
                    stdout.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                    stderr.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                ]
                .into_iter()
                .flatten()
                {
                    let fail_tx = self.browser_fail_tx.clone();
                    thread::spawn(move || {
                        let reader = BufReader::new(stream);
                        let mut launched = false;
                        for line in reader.lines().map_while(Result::ok) {
                            println!("{}", line);
                            if !launched {
                                if let Some(start) = line.find("http://") {
                                    let url: String = line[start..]
                                        .chars()
                                        .take_while(|c| !c.is_whitespace())
                                        .collect();
                                    println!("Opening {} in firefox", url);
                                    // Watch the spawned firefox: a non-zero
                                    // exit means it could not open (typically
                                    // "already running" — the shared profile
                                    // is locked by another analysis machine);
                                    // report it so the UI can scan for WHERE.
                                    match Command::new("firefox").arg(&url).spawn() {
                                        Ok(mut ff) => {
                                            let tx = fail_tx.clone();
                                            thread::spawn(move || {
                                                if let Ok(status) = ff.wait() {
                                                    if !status.success() {
                                                        let _ = tx.send(
                                                            "Firefox could not open the notebook"
                                                                .to_string(),
                                                        );
                                                    }
                                                }
                                            });
                                        }
                                        Err(e) => {
                                            let _ = fail_tx.send(format!(
                                                "Could not start firefox: {e}"
                                            ));
                                        }
                                    }
                                    launched = true;
                                }
                            }
                        }
                    });
                }
                // Detach: keep child running after we drop the handle.
                std::mem::forget(child);
                self.launch_time = Some(Instant::now());
                self.launch_status = Some((
                    format!("Provisioned {}", dest.display()),
                    theme::SUCCESS,
                ));
                // Warn NOW when the shared profile is locked by another
                // machine: the firefox we are about to spawn will only sit
                // on its own "already running" dialog (and may never exit,
                // so the failure→scan path would never fire).
                if let Some(report) = firefox_remote_lock_report() {
                    self.scan_output = Some(report);
                    self.scan_window_open = true;
                    self.scan_window_kind = ScanWindowKind::Scan;
                    self.launch_status = Some((
                        "Your browser is already running on another machine \
                         (see the report window)"
                            .to_string(),
                        theme::DANGER,
                    ));
                }
            }
            Err(e) => {
                self.launch_status =
                    Some((format!("Failed to launch {}: {}", marimo_bin, e), theme::DANGER));
            }
        }
    }

    /// Run the maintenance script that kills stuck browser sessions.
    ///
    /// The script's output is captured and shown in the report window once
    /// it finishes: users need to see whether anything was actually killed,
    /// and on which machine — a silent run that reached no host (no SSH key
    /// set up, hosts refusing the login) used to look exactly like success.
    fn kill_stuck_browsers(&mut self) {
        if self.cleanup_rx.is_some() {
            return; // one kill run at a time
        }
        if !Path::new(FIX_BROWSER_SCRIPT).is_file() {
            self.cleanup_msg = Some((
                format!("Missing script {}", FIX_BROWSER_SCRIPT),
                theme::DANGER,
            ));
            return;
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let report = match Command::new(FIX_BROWSER_SCRIPT)
                .arg("kill")
                .arg("-v")
                .output()
            {
                Ok(out) => {
                    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                    let err = String::from_utf8_lossy(&out.stderr);
                    if !err.trim().is_empty() {
                        text.push_str("\n");
                        text.push_str(err.trim_end());
                    }
                    text
                }
                Err(e) => format!("Could not run {}: {e}", FIX_BROWSER_SCRIPT),
            };
            let _ = tx.send(report);
        });
        self.cleanup_rx = Some(rx);
        self.cleanup_msg = Some((
            "Killing stuck browser sessions on the analysis machines\u{2026}".to_string(),
            theme::WARNING,
        ));
    }

    /// The kill run finished: summarise it next to the button and pop the
    /// full report so the user sees what happened on which machine.
    fn finish_kill_stuck_browsers(&mut self, report: String) {
        let s = parse_kill_summary(&report);
        let (msg, color) = if s.auth_failed {
            (
                "Could not log into the analysis machines (SSH refused) — see the report window"
                    .to_string(),
                theme::DANGER,
            )
        } else if s.found == 0 && s.clean == 0 && s.unreachable > 0 {
            (
                "No analysis machine could be reached — see the report window".to_string(),
                theme::DANGER,
            )
        } else if s.found == 0 {
            (
                "No browser or Jupyter of yours found running on the analysis machines"
                    .to_string(),
                theme::SUCCESS,
            )
        } else if s.remaining == 0 {
            (
                format!(
                    "Killed {} process(es) on {} machine(s) — you can launch again",
                    s.found, s.hosts
                ),
                theme::SUCCESS,
            )
        } else {
            (
                format!(
                    "{} process(es) still running after the kill — see the report window",
                    s.remaining
                ),
                theme::DANGER,
            )
        };
        self.cleanup_msg = Some((msg, color));
        self.scan_output = Some(report);
        self.scan_window_open = true;
        self.scan_window_kind = ScanWindowKind::Kill;
    }

    /// Firefox failed to open: run `FIX_BROWSER_SCRIPT list -firefox` in a
    /// background thread to find on which analysis machine the browser (or
    /// the Jupyter keeping it alive) is already running.
    fn start_browser_scan(&mut self, why: &str) {
        if self.scan_rx.is_some() {
            return; // one scan at a time
        }
        // The profile lock symlink answers instantly when present; only
        // fall back to the (slow, SSH-based) scan when it says nothing.
        if let Some(report) = firefox_remote_lock_report() {
            self.scan_output = Some(report);
            self.scan_window_open = true;
            self.scan_window_kind = ScanWindowKind::Scan;
            self.launch_status = Some((
                format!("{why} — your browser is running on another machine (see the report window)"),
                theme::DANGER,
            ));
            return;
        }
        if !Path::new(FIX_BROWSER_SCRIPT).is_file() {
            self.launch_status = Some((
                format!("{why} — your browser may be running on another analysis machine"),
                theme::DANGER,
            ));
            return;
        }
        self.launch_status = Some((
            format!("{why} — scanning the analysis machines for an already-running browser\u{2026}"),
            theme::WARNING,
        ));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let report = match Command::new(FIX_BROWSER_SCRIPT)
                .arg("list")
                .arg("-firefox")
                .output()
            {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    if stdout.trim().is_empty() {
                        String::from_utf8_lossy(&out.stderr).into_owned()
                    } else {
                        // Drop the script's closing "Re-run with 'kill'" hint:
                        // the window advises closing the browser on the listed
                        // machine instead (remote kills are unreliable).
                        match stdout.find("Re-run with 'kill'") {
                            Some(pos) => stdout[..pos].trim_end().to_owned(),
                            None => stdout.into_owned(),
                        }
                    }
                }
                Err(e) => format!("Could not run the scan script: {e}"),
            };
            let _ = tx.send(report);
        });
        self.scan_rx = Some(rx);
    }

    /// Poll the firefox-failure and scan channels; pop the report window when
    /// the scan found the user's browser running somewhere.
    fn poll_browser_scan(&mut self) {
        if let Ok(why) = self.browser_fail_rx.try_recv() {
            self.start_browser_scan(&why);
        }
        let Some(rx) = &self.scan_rx else { return };
        let Ok(report) = rx.try_recv() else { return };
        self.scan_rx = None;
        if scan_found_processes(&report) {
            self.scan_window_open = true;
            self.scan_window_kind = ScanWindowKind::Scan;
            self.launch_status = Some((
                "Your browser is already running on another machine (see the report window)"
                    .to_string(),
                theme::DANGER,
            ));
        } else {
            self.launch_status = Some((
                "Firefox failed, but no already-running browser was found on the analysis machines"
                    .to_string(),
                theme::WARNING,
            ));
        }
        self.scan_output = Some(report);
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Theme is installed once at startup (see `theme::apply`).

        // Firefox-failure watcher / cross-machine browser scan.
        self.poll_browser_scan();

        // (Re)load the logo whenever the selected instrument's logo isn't the
        // one on screen — first frame and after an instrument switch.
        if self.logo_instrument != Some(self.instrument) {
            self.logo = Logo::load(ctx, INSTRUMENTS[self.instrument].2);
            self.logo_instrument = Some(self.instrument);
        }

        // Branded header (Coefficient "Header" pattern, branding slot): a
        // full-width rich ORNL Green banner with a white title provides strong
        // brand presence, with the VENUS imaging logo in the top-right corner.
        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(theme::PRIMARY_RICH)
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: 8,
                        bottom: 8,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Title with a soft drop shadow: egui has no text shadow, so
                    // paint the text twice — a dark offset copy behind the white.
                    let title = format!("{} General Tools", INSTRUMENTS[self.instrument].0);
                    let title = title.as_str();
                    let font = egui::FontId::proportional(28.0);
                    let shadow_offset = egui::vec2(2.0, 2.0);
                    let galley = ui.painter().layout_no_wrap(
                        title.to_string(),
                        font.clone(),
                        theme::TEXT_WHITE,
                    );
                    let (rect, _) = ui.allocate_exact_size(
                        galley.size() + shadow_offset,
                        egui::Sense::hover(),
                    );
                    let pos = rect.min;
                    ui.painter().text(
                        pos + shadow_offset,
                        egui::Align2::LEFT_TOP,
                        title,
                        font.clone(),
                        egui::Color32::from_black_alpha(140),
                    );
                    ui.painter().text(
                        pos,
                        egui::Align2::LEFT_TOP,
                        title,
                        font,
                        theme::TEXT_WHITE,
                    );
                    if let Some(logo) = &self.logo {
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let height = 44.0;
                                let size = egui::vec2(height * logo.aspect, height);
                                let (rect, _) =
                                    ui.allocate_exact_size(size, egui::Sense::hover());
                                let uv = egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                );
                                let shadow_offset = egui::vec2(2.0, 2.0);
                                // Drop shadow: the texture tinted black draws its
                                // alpha as a dark silhouette behind the logo.
                                ui.painter().image(
                                    logo.texture.id(),
                                    rect.translate(shadow_offset),
                                    uv,
                                    egui::Color32::from_black_alpha(140),
                                );
                                ui.painter().image(
                                    logo.texture.id(),
                                    rect,
                                    uv,
                                    egui::Color32::WHITE,
                                );
                            },
                        );
                    }
                });
            });

        // Instrument selector, directly under the header: switching instrument
        // rescans that instrument's root and rebuilds the IPTS list.
        egui::TopBottomPanel::top("instrument_bar")
            .frame(
                egui::Frame::new()
                    .fill(theme::surface_weak(&ctx.style().visuals))
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: 8,
                        bottom: 8,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::section_heading("Instrument:"));
                    let mut changed = false;
                    for (i, (name, _, _)) in INSTRUMENTS.iter().enumerate() {
                        if ui
                            .selectable_label(self.instrument == i, *name)
                            .clicked()
                            && self.instrument != i
                        {
                            self.instrument = i;
                            changed = true;
                        }
                    }
                    if changed {
                        self.reload_ipts();
                        // Keep the OS window title in sync with the header.
                        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                            "{} General Tools",
                            INSTRUMENTS[self.instrument].0
                        )));
                    }
                    ui.separator();
                    theme::toggle_button(ui);
                    zoom::toggle_button(ui);
                });
            });

        // Bottom panel with launch button (spans the full width).
        egui::TopBottomPanel::bottom("bottom_panel")
            .frame(
                egui::Frame::new()
                    .fill(theme::surface_base(&ctx.style().visuals))
                    .inner_margin(12.0),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    let launching = self
                        .launch_time
                        .map(|t| t.elapsed().as_secs() < 5)
                        .unwrap_or(false);

                    let ready = self
                        .selected
                        .map_or(false, |idx| self.app_available(idx))
                        && self.ipts_selected.is_some();

                    if launching {
                        ui.add_enabled(
                            false,
                            theme::primary_button("\u{231b} Launching\u{2026}"),
                        );
                        ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    } else if ready {
                        // Primary action: ORNL Green, title-case label, no punctuation.
                        if ui.add(theme::primary_button("Launch Application")).clicked() {
                            self.launch();
                        }
                    } else {
                        ui.add_enabled(
                            false,
                            egui::Button::new("Select an IPTS and an Application"),
                        );
                    }

                    if let Some((msg, color)) = &self.launch_status {
                        ui.colored_label(*color, msg);
                    }
                });

                // Report the kill run once the script has finished.
                if let Some(rx) = &self.cleanup_rx {
                    match rx.try_recv() {
                        Ok(report) => {
                            self.cleanup_rx = None;
                            self.finish_kill_stuck_browsers(report);
                        }
                        Err(mpsc::TryRecvError::Disconnected) => {
                            self.cleanup_rx = None;
                            self.cleanup_msg = None;
                        }
                        Err(mpsc::TryRecvError::Empty) => {
                            ctx.request_repaint_after(std::time::Duration::from_millis(200));
                        }
                    }
                }

                // Bottom-left corner utility: kill stuck browser sessions.
                // Same button as the Jupyter notebooks portal.
                // Overlaid with `put` so the primary action stays centered;
                // vertically centered on the primary button's row (36 px tall).
                let btn_size = egui::vec2(150.0, 28.0);
                let btn_rect = egui::Rect::from_center_size(
                    egui::pos2(
                        ui.min_rect().left() + btn_size.x / 2.0,
                        ui.min_rect().top() + 18.0,
                    ),
                    btn_size,
                );
                let resp = ui
                    .put(
                        btn_rect,
                        egui::Button::new(
                            egui::RichText::new("\u{1F527} Fix browser issue")
                                .color(theme::TEXT_WHITE),
                        )
                        .fill(egui::Color32::from_rgb(138, 43, 226))
                        .corner_radius(6.0),
                    )
                    .on_hover_text("Kill stuck browser sessions");
                if resp.clicked() {
                    self.kill_stuck_browsers();
                }
                // The kill summary gets its own row under the buttons: painted
                // next to the Fix button it ran into the centered Launch
                // button, and a painter overlay reserves no panel height.
                if let Some((msg, color)) = &self.cleanup_msg {
                    ui.add_space(4.0);
                    ui.colored_label(*color, msg);
                }
            });

        // Left panel: IPTS selection.
        egui::SidePanel::left("ipts_panel")
            .resizable(false)
            .exact_width(300.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::surface_base(&ctx.style().visuals))
                    .inner_margin(12.0),
            )
            .show(ctx, |ui| {
                ui.label(theme::section_heading("Select your IPTS"));

                if let Some(err) = &self.ipts_error {
                    ui.colored_label(theme::DANGER, err);
                }
                let writable_count = self.ipts_entries.iter().filter(|e| e.writable).count();
                ui.colored_label(
                    theme::text_emphasis(ui.visuals()),
                    format!(
                        "You have write access to {} IPTS at {}",
                        writable_count,
                        INSTRUMENTS[self.instrument].0
                    ),
                );

                // Manual IPTS entry.
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("IPTS-").strong());
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.manual_ipts)
                            .desired_width(120.0)
                            .hint_text("number"),
                    );
                    if resp.changed() {
                        let trimmed = self.manual_ipts.trim().to_string();
                        if trimmed.is_empty() {
                            self.manual_ipts_msg = None;
                        } else if trimmed.parse::<u64>().is_err() {
                            self.manual_ipts_msg =
                                Some(("Enter digits only".to_string(), theme::DANGER));
                        } else {
                            let target = format!("IPTS-{}", trimmed);
                            let found = self
                                .ipts_entries
                                .iter()
                                .enumerate()
                                .find(|(_, e)| e.label == target)
                                .map(|(idx, e)| (idx, e.writable));
                            match found {
                                Some((idx, true)) => {
                                    self.ipts_selected = Some(idx);
                                    self.scroll_to_ipts = true;
                                    self.manual_ipts_msg = None;
                                    self.launch_status = None;
                                }
                                Some((_, false)) => {
                                    self.manual_ipts_msg = Some((
                                        format!("{} found but no write access", target),
                                        theme::WARNING,
                                    ));
                                }
                                None => {
                                    self.manual_ipts_msg =
                                        Some((format!("{} not found", target), theme::DANGER));
                                }
                            }
                        }
                    }
                });
                if let Some((msg, color)) = &self.manual_ipts_msg {
                    ui.colored_label(*color, msg);
                }

                ui.add_space(6.0);

                // IPTS list fills the remaining panel height.
                theme::container_frame(ui.visuals())
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .id_salt("ipts_scroll")
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                for i in 0..self.ipts_entries.len() {
                                    let (label, writable) = {
                                        let e = &self.ipts_entries[i];
                                        (e.label.clone(), e.writable)
                                    };
                                    let is_selected = self.ipts_selected == Some(i);
                                    if writable {
                                        let resp = ui.selectable_label(is_selected, &label);
                                        if resp.clicked() {
                                            self.ipts_selected = Some(i);
                                            self.manual_ipts = label
                                                .strip_prefix("IPTS-")
                                                .unwrap_or("")
                                                .to_string();
                                            self.manual_ipts_msg = None;
                                            self.launch_status = None;
                                        }
                                        if is_selected && self.scroll_to_ipts {
                                            resp.scroll_to_me(Some(egui::Align::Center));
                                        }
                                    } else {
                                        ui.add_enabled(
                                            false,
                                            egui::SelectableLabel::new(false, &label),
                                        )
                                        .on_disabled_hover_text(
                                            "No write access to shared folder",
                                        );
                                    }
                                }
                                self.scroll_to_ipts = false;
                            });
                    });
            });

        // Central panel: application (tools) selection.
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);

            let prev_selected = self.selected;

            ui.label(theme::section_heading("Select your application"));

            // Application list.
            theme::container_frame(ui.visuals())
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(260.0)
                        .id_salt("app_scroll")
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let mut clicked = None;
                            let instrument = INSTRUMENTS[self.instrument].0;
                            let mut app_row = |ui: &mut egui::Ui, i: usize| {
                                let name = &self.applications[i].0;
                                if !self.app_available(i) {
                                    // Not for this instrument: grayed out,
                                    // not selectable.
                                    ui.add_enabled(
                                        false,
                                        egui::SelectableLabel::new(false, name),
                                    )
                                    .on_disabled_hover_text(format!(
                                        "Not available at {}",
                                        instrument
                                    ));
                                    return;
                                }
                                if ui
                                    .selectable_label(self.selected == Some(i), name)
                                    .clicked()
                                {
                                    clicked = Some(i);
                                }
                            };
                            for item in &self.app_display {
                                match item {
                                    DisplayItem::App(i) => app_row(ui, *i),
                                    DisplayItem::Section(category, indices) => {
                                        egui::CollapsingHeader::new(
                                            egui::RichText::new(category).strong(),
                                        )
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            for &i in indices {
                                                app_row(ui, i);
                                            }
                                        });
                                    }
                                }
                            }
                            if let Some(i) = clicked {
                                self.selected = Some(i);
                                self.launch_status = None;
                            }
                        });
                });

            // Load screenshot when selection changes
            if self.selected != prev_selected {
                if let Some(idx) = self.selected {
                    let screenshot_path = self.applications[idx].1.screenshot.clone();
                    self.load_screenshot(ctx, &screenshot_path);
                } else {
                    self.screenshot_texture = None;
                }
            }

            // Description box
            if let Some(idx) = self.selected {
                let desc = &self.applications[idx].1.description;
                if !desc.is_empty() {
                    ui.add_space(5.0);
                    egui::Frame::new()
                        .fill(theme::surface_container(ui.visuals()))
                        .stroke(egui::Stroke::new(1.0, theme::border_subtle(ui.visuals())))
                        .corner_radius(6.0)
                        .inner_margin(12.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(desc);
                        });
                }

                // Screenshot
                if let Some(texture) = &self.screenshot_texture {
                    ui.add_space(5.0);
                    let available_width = ui.available_width();
                    let tex_size = texture.size_vec2();
                    let scale = (available_width / tex_size.x).min(1.0);
                    let display_size = egui::vec2(tex_size.x * scale, tex_size.y * scale);
                    egui::ScrollArea::vertical()
                        .id_salt("screenshot_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.image(egui::load::SizedTexture::new(texture.id(), display_size));
                        });
                }
            }
        });

        // ------------------------- browser-running-elsewhere scan report ---
        if self.scan_window_open {
            let mut open = true;
            let kind = self.scan_window_kind;
            let (title, intro, advice) = match kind {
                ScanWindowKind::Scan => (
                    "Browser already running elsewhere",
                    "Firefox cannot open the notebook on this machine: \
                     your browser profile is on shared storage and is \
                     locked by a session on the machine(s) listed below.",
                    "Click \u{1F527} Fix browser issue to close it from here, \
                     or log into that machine and close the browser (and \
                     any Jupyter) there, then launch again.",
                ),
                ScanWindowKind::Kill => (
                    "Fix browser issue — result",
                    "Your Firefox / Jupyter processes were looked for on \
                     every analysis machine and killed where found. Machines \
                     marked \"unreachable\" could not be logged into, so \
                     nothing was checked or killed there.",
                    "If your browser still refuses to open after this, log \
                     into the listed machine and close it there.",
                ),
            };
            egui::Window::new(title)
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_size([680.0, 440.0])
                .show(ctx, |ui| {
                    ui.label(egui::RichText::new(intro).size(16.0));
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(advice)
                            .size(16.0)
                            .color(theme::text_emphasis(ui.visuals())),
                    );
                    ui.add_space(8.0);
                    ui.separator();
                    egui::ScrollArea::both()
                        .id_salt("browser_scan_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(
                                    self.scan_output.as_deref().unwrap_or(""),
                                )
                                .monospace()
                                .size(15.0),
                            );
                        });
                });
            if !open {
                self.scan_window_open = false;
            }
        }

        // Keep polling the failure/scan channels without mouse movement: the
        // spawned firefox can fail well after the 5 s "Launching…" period.
        if self.launch_time.is_some() || self.scan_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
    }
}
