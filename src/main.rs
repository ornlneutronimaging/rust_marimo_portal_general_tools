mod theme;

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
/// Maintenance script that lists and kills stuck browser sessions.
const FIX_BROWSER_SCRIPT: &str = "/SNS/VENUS/shared/software/bin/list_and_fix_running_browser.sh";

struct AppEntry {
    description: String,
    path: String,
    marimo_path: String,
    screenshot: String,
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
            (
                name,
                AppEntry {
                    description,
                    path,
                    marimo_path,
                    screenshot,
                },
            )
        })
        .collect()
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

    Ok(PathBuf::from(file_name))
}

fn main() -> eframe::Result {
    let mut options = eframe::NativeOptions::default();
    options.viewport = options.viewport.with_inner_size(egui::vec2(880.0, 780.0));
    eframe::run_native(
        &format!("{} General Tools", INSTRUMENTS[0].0),
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(MyApp::new()))
        }),
    )
}

struct MyApp {
    applications: Vec<(String, AppEntry)>,
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
    /// Signals when the cleanup script exits, so the message can be cleared.
    cleanup_rx: Option<mpsc::Receiver<()>>,
}

impl MyApp {
    fn new() -> Self {
        let mut app = Self {
            applications: load_applications(),
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
        };
        app.reload_ipts();
        app
    }

    /// Rebuild the IPTS list from the selected instrument's root and clear any
    /// selection/state tied to the previous instrument.
    fn reload_ipts(&mut self) {
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
                                    let _ = Command::new("firefox").arg(&url).spawn();
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
            }
            Err(e) => {
                self.launch_status =
                    Some((format!("Failed to launch {}: {}", marimo_bin, e), theme::DANGER));
            }
        }
    }

    /// Run the maintenance script that kills stuck browser sessions.
    fn kill_stuck_browsers(&mut self) {
        match Command::new(FIX_BROWSER_SCRIPT).arg("kill").spawn() {
            Ok(mut child) => {
                // Reap the child in the background and signal the UI when it
                // exits so the status message can be cleared.
                let (tx, rx) = mpsc::channel();
                thread::spawn(move || {
                    let _ = child.wait();
                    let _ = tx.send(());
                });
                self.cleanup_rx = Some(rx);
                self.cleanup_msg = Some((
                    "Killing stuck browser sessions\u{2026}".to_string(),
                    theme::SUCCESS,
                ));
            }
            Err(e) => {
                self.cleanup_rx = None;
                self.cleanup_msg = Some((
                    format!("Failed to run {}: {}", FIX_BROWSER_SCRIPT, e),
                    theme::DANGER,
                ));
            }
        }
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Theme is installed once at startup (see `theme::apply`).

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
                    .fill(theme::SURFACE_WEAK)
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
                });
            });

        // Bottom panel with launch button (spans the full width).
        egui::TopBottomPanel::bottom("bottom_panel")
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE_BASE)
                    .inner_margin(12.0),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    let launching = self
                        .launch_time
                        .map(|t| t.elapsed().as_secs() < 5)
                        .unwrap_or(false);

                    let ready = self.selected.is_some() && self.ipts_selected.is_some();

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

                // Clear the cleanup message once the script has finished.
                if let Some(rx) = &self.cleanup_rx {
                    if rx.try_recv().is_ok() {
                        self.cleanup_rx = None;
                        self.cleanup_msg = None;
                    } else {
                        ctx.request_repaint_after(std::time::Duration::from_millis(200));
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
                if let Some((msg, color)) = &self.cleanup_msg {
                    ui.painter().text(
                        btn_rect.right_center() + egui::vec2(8.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        msg,
                        egui::TextStyle::Body.resolve(ui.style()),
                        *color,
                    );
                }
            });

        // Left panel: IPTS selection.
        egui::SidePanel::left("ipts_panel")
            .resizable(false)
            .exact_width(300.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE_BASE)
                    .inner_margin(12.0),
            )
            .show(ctx, |ui| {
                ui.label(theme::section_heading("Select your IPTS"));

                if let Some(err) = &self.ipts_error {
                    ui.colored_label(theme::DANGER, err);
                }
                let writable_count = self.ipts_entries.iter().filter(|e| e.writable).count();
                ui.colored_label(
                    theme::TEXT_EMPHASIS,
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
                theme::container_frame()
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
            theme::container_frame()
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(260.0)
                        .id_salt("app_scroll")
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for (i, (name, _)) in self.applications.iter().enumerate() {
                                if ui
                                    .selectable_label(self.selected == Some(i), name)
                                    .clicked()
                                {
                                    self.selected = Some(i);
                                    self.launch_status = None;
                                }
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
                        .fill(theme::SURFACE_CONTAINER)
                        .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
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
    }
}
