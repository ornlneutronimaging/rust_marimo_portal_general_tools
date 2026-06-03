use eframe::egui;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Instant;

const JSON_PATH: &str =
    "/SNS/VENUS/shared/software/menu/list_marimo_general_users_applications.json";

struct AppEntry {
    description: String,
    path: String,
    marimo_path: String,
    screenshot: String,
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

fn main() -> eframe::Result {
    let mut options = eframe::NativeOptions::default();
    options.viewport = options.viewport.with_inner_size(egui::vec2(500.0, 1100.0));
    eframe::run_native(
        "General Tools",
        options,
        Box::new(|_cc| Ok(Box::new(MyApp::new()))),
    )
}

struct MyApp {
    applications: Vec<(String, AppEntry)>,
    selected: Option<usize>,
    launch_time: Option<Instant>,
    screenshot_texture: Option<egui::TextureHandle>,
}

impl MyApp {
    fn new() -> Self {
        Self {
            applications: load_applications(),
            selected: None,
            launch_time: None,
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
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::BLACK;
        visuals.window_fill = egui::Color32::BLACK;
        ctx.set_visuals(visuals);

        let mut style = (*ctx.style()).clone();
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(16.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(20.0));
        style.spacing.item_spacing = egui::vec2(8.0, 12.0);
        style.spacing.button_padding = egui::vec2(16.0, 8.0);
        ctx.set_style(style);

        // Bottom panel with launch button
        if self.selected.is_some() {
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

                        if launching {
                            ui.add_enabled(false, egui::Button::new("\u{231b} Launching..."));
                            ctx.request_repaint_after(std::time::Duration::from_millis(100));
                        } else if ui.button("Launch this application").clicked() {
                            if let Some(idx) = self.selected {
                                let app = &self.applications[idx];
                                let marimo_bin = &app.1.marimo_path;
                                println!("Launching: {} run {}", marimo_bin, &app.1.path);
                                match Command::new(marimo_bin)
                                    .arg("run")
                                    .arg(&app.1.path)
                                    .arg("--headless")
                                    .stdout(Stdio::piped())
                                    .stderr(Stdio::piped())
                                    .spawn()
                                {
                                    Ok(mut child) => {
                                        let stdout = child.stdout.take();
                                        let stderr = child.stderr.take();
                                        for stream in [stdout.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), stderr.map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)].into_iter().flatten() {
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
                                    }
                                    Err(e) => eprintln!("Failed to launch {}: {}", marimo_bin, e),
                                }
                                self.launch_time = Some(Instant::now());
                            }
                        }
                    });
                });
        }

        // Central panel
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(10.0);

            let prev_selected = self.selected;

            ui.strong("Select your application");

            // Application list
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
                        .min_scrolled_height(400.0)
                        .max_height(400.0)
                        .show(ui, |ui| {
                            ui.set_height(400.0);
                            ui.set_width(ui.available_width());
                            for (i, (name, _)) in self.applications.iter().enumerate() {
                                if ui
                                    .selectable_label(self.selected == Some(i), name)
                                    .clicked()
                                {
                                    self.selected = Some(i);
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
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.image(egui::load::SizedTexture::new(texture.id(), display_size));
                    });
                }
            }
        });
    }
}
