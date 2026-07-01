use eframe::egui;
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Instant;

const JSON_PATH: &str =
    "/SNS/VENUS/shared/software/menu/list_marimo_general_users_applications.json";
const VENUS_ROOT: &str = "/SNS/VENUS";
// Directories that must never be copied into the user's IPTS folder.
const SKIP_DIRS: &[&str] = &["__pycache__", "__marimo__"];

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

/// `/SNS/VENUS/<ipts>/shared/notebooks/imaging_marimo_<user>`
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
        "General Tools",
        options,
        Box::new(|_cc| Ok(Box::new(MyApp::new()))),
    )
}

struct MyApp {
    applications: Vec<(String, AppEntry)>,
    selected: Option<usize>,
    ipts_entries: Vec<IptsEntry>,
    ipts_error: Option<String>,
    ipts_selected: Option<usize>,
    manual_ipts: String,
    manual_ipts_msg: Option<(String, egui::Color32)>,
    scroll_to_ipts: bool,
    launch_time: Option<Instant>,
    launch_status: Option<(String, egui::Color32)>,
    screenshot_texture: Option<egui::TextureHandle>,
}

impl MyApp {
    fn new() -> Self {
        let (ipts_entries, ipts_error) = match list_ipts(Path::new(VENUS_ROOT)) {
            Ok(list) => (list, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        Self {
            applications: load_applications(),
            selected: None,
            ipts_entries,
            ipts_error,
            ipts_selected: None,
            manual_ipts: String::new(),
            manual_ipts_msg: None,
            scroll_to_ipts: false,
            launch_time: None,
            launch_status: None,
            screenshot_texture: None,
        }
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
                self.launch_status = Some((e, egui::Color32::RED));
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
                    egui::Color32::from_rgb(46, 160, 67),
                ));
            }
            Err(e) => {
                self.launch_status =
                    Some((format!("Failed to launch {}: {}", marimo_bin, e), egui::Color32::RED));
            }
        }
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::BLACK;
        visuals.window_fill = egui::Color32::BLACK;
        ctx.set_visuals(visuals);

        // Use egui's default (small) fonts, matching the jupyter reference portal.

        // Bottom panel with launch button (spans the full width).
        egui::TopBottomPanel::bottom("bottom_panel")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::BLACK)
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
                        ui.add_enabled(false, egui::Button::new("\u{231b} Launching..."));
                        ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    } else if ready {
                        if ui.button("Launch this application").clicked() {
                            self.launch();
                        }
                    } else {
                        ui.add_enabled(
                            false,
                            egui::Button::new("Select an IPTS and an application"),
                        );
                    }

                    if let Some((msg, color)) = &self.launch_status {
                        ui.colored_label(*color, msg);
                    }
                });
            });

        // Left panel: IPTS selection.
        egui::SidePanel::left("ipts_panel")
            .resizable(false)
            .exact_width(300.0)
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::BLACK)
                    .inner_margin(12.0),
            )
            .show(ctx, |ui| {
                ui.strong("Select your IPTS");

                if let Some(err) = &self.ipts_error {
                    ui.colored_label(egui::Color32::RED, err);
                }
                let writable_count = self.ipts_entries.iter().filter(|e| e.writable).count();
                ui.label(format!(
                    "You have write access to {} IPTS at VENUS",
                    writable_count
                ));

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
                                Some(("Enter digits only".to_string(), egui::Color32::RED));
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
                                        egui::Color32::from_rgb(200, 120, 0),
                                    ));
                                }
                                None => {
                                    self.manual_ipts_msg =
                                        Some((format!("{} not found", target), egui::Color32::RED));
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
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(25, 25, 30))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 80, 100)))
                    .corner_radius(4.0)
                    .inner_margin(4.0)
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

            ui.strong("Select your application");

            // Application list.
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(25, 25, 30))
                .stroke(egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgb(80, 80, 100),
                ))
                .corner_radius(4.0)
                .inner_margin(4.0)
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
                        .fill(egui::Color32::from_rgb(25, 25, 30))
                        .stroke(egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(80, 80, 100),
                        ))
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
