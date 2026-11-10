#![windows_subsystem = "windows"]

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use eframe::egui;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const SETTINGS_FILE: &str = "vault-gui.txt";
const RUNNABLE: [&str; 3] = ["exe", "bat", "cmd"];

enum Message {
    Line(String),
    Done(String),
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Source,
    Drive,
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

struct App {
    vault_text: String,
    vault: Option<PathBuf>,
    environments: Vec<String>,
    selected: Option<String>,
    tab: Tab,
    new_name: String,
    args: String,
    log: Vec<String>,
    running: Option<(String, Receiver<Message>)>,
    listing: HashMap<PathBuf, Vec<Entry>>,
}

fn settings_path() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.with_file_name(SETTINGS_FILE))
}

fn is_runnable(path: &Path) -> bool {
    path.extension().is_some_and(|extension| RUNNABLE.iter().any(|known| extension.eq_ignore_ascii_case(known)))
}

fn needs_console(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd"))
}

fn split_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

fn open_in_explorer(path: &Path) {
    let _ = Command::new("explorer").arg(path).spawn();
}

fn forward_lines(pipe: impl Read + Send + 'static, sender: Sender<Message>, ctx: egui::Context) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            let _ = sender.send(Message::Line(line));
            ctx.request_repaint();
        }
    })
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            vault_text: String::new(),
            vault: None,
            environments: Vec::new(),
            selected: None,
            tab: Tab::Source,
            new_name: String::new(),
            args: String::new(),
            log: Vec::new(),
            running: None,
            listing: HashMap::new(),
        };
        if let Some(saved) = settings_path().and_then(|path| std::fs::read_to_string(path).ok()) {
            app.vault_text = saved.trim().to_owned();
            if !app.vault_text.is_empty() {
                app.connect();
            }
        }
        app
    }

    fn connect(&mut self) {
        let path = PathBuf::from(self.vault_text.trim());
        if !path.join("tools").join("vault-cli.exe").is_file() {
            self.log.push(format!("в {} нет tools\\vault-cli.exe: это не хранилище", path.display()));
            self.vault = None;
            return;
        }
        if let Some(settings) = settings_path() {
            let _ = std::fs::write(settings, path.to_string_lossy().as_bytes());
        }
        self.log.push(format!("подключено: {}", path.display()));
        self.vault = Some(path);
        self.refresh();
    }

    fn refresh(&mut self) {
        self.listing.clear();
        self.environments.clear();
        let Some(vault) = &self.vault else { return };
        if let Ok(entries) = std::fs::read_dir(vault.join("plugins")) {
            self.environments = entries
                .flatten()
                .filter(|entry| entry.path().join("environment.txt").is_file())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
        }
        self.environments.sort();
        if self.selected.as_ref().is_some_and(|name| !self.environments.contains(name)) {
            self.selected = None;
        }
    }

    fn environment_folder(&self, name: &str) -> Option<PathBuf> {
        Some(self.vault.as_ref()?.join("plugins").join(name))
    }

    fn list(&mut self, folder: &Path) -> Vec<Entry> {
        self.listing
            .entry(folder.to_path_buf())
            .or_insert_with(|| {
                let mut entries: Vec<Entry> = std::fs::read_dir(folder)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| Entry {
                        path: entry.path(),
                        name: entry.file_name().to_string_lossy().into_owned(),
                        is_dir: entry.file_type().is_ok_and(|kind| kind.is_dir()),
                    })
                    .collect();
                entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
                entries
            })
            .clone()
    }

    fn start(&mut self, label: String, args: Vec<OsString>, console: bool, ctx: &egui::Context) {
        let Some(vault) = self.vault.clone() else { return };
        let cli = vault.join("tools").join("vault-cli.exe");
        let (sender, receiver) = channel();
        let ctx = ctx.clone();
        self.log.push(format!("> {label}"));
        self.running = Some((label, receiver));
        thread::spawn(move || {
            let mut args = args.into_iter();
            let mut full: Vec<OsString> = args.next().into_iter().collect();
            full.push("--vault".into());
            full.push(vault.into_os_string());
            full.extend(args);
            let result = if console {
                let quoted: Vec<String> = std::iter::once(cli.into_os_string())
                    .chain(full)
                    .map(|part| format!("\"{}\"", part.to_string_lossy()))
                    .collect();
                Command::new("cmd.exe")
                    .raw_arg(format!("/c \"{} & echo. & pause\"", quoted.join(" ")))
                    .creation_flags(CREATE_NEW_CONSOLE)
                    .status()
                    .map(|status| format!("консоль закрыта, код {}", status.code().unwrap_or(-1)))
            } else {
                Command::new(&cli)
                    .args(&full)
                    .creation_flags(CREATE_NO_WINDOW)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .and_then(|mut child| {
                        let readers = [
                            child.stdout.take().map(|pipe| forward_lines(pipe, sender.clone(), ctx.clone())),
                            child.stderr.take().map(|pipe| forward_lines(pipe, sender.clone(), ctx.clone())),
                        ];
                        let status = child.wait()?;
                        for reader in readers.into_iter().flatten() {
                            let _ = reader.join();
                        }
                        Ok(format!("готово, код {}", status.code().unwrap_or(-1)))
                    })
            };
            let _ = sender.send(Message::Done(result.unwrap_or_else(|e| format!("не запустилось: {e}"))));
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        let Some((_, receiver)) = &self.running else { return };
        let mut finished = None;
        while let Ok(message) = receiver.try_recv() {
            match message {
                Message::Line(line) => self.log.push(line),
                Message::Done(summary) => finished = Some(summary),
            }
        }
        if let Some(summary) = finished {
            self.log.push(summary);
            self.running = None;
            self.refresh();
        }
    }

    fn create(&mut self, ctx: &egui::Context) {
        let name = self.new_name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        self.start(format!("create {name}"), vec!["create".into(), "--name".into(), name.clone().into()], false, ctx);
        self.selected = Some(name);
        self.new_name.clear();
    }

    fn pull(&mut self, name: &str, from: PathBuf, ctx: &egui::Context) {
        let label = format!("pull {} в {name}", from.display());
        self.start(label, vec!["pull".into(), "--name".into(), name.into(), from.into_os_string()], false, ctx);
    }

    fn run(&mut self, name: &str, program: &Path, ctx: &egui::Context) {
        let mut args: Vec<OsString> = vec!["run".into(), "--name".into(), name.into(), program.as_os_str().to_owned()];
        args.extend(split_args(&self.args).into_iter().map(OsString::from));
        let label = format!("run {} в {name}", program.file_name().unwrap_or_default().to_string_lossy());
        self.start(label, args, needs_console(program), ctx);
    }

    fn tree(&mut self, ui: &mut egui::Ui, folder: &Path, can_run: bool) -> Option<PathBuf> {
        let mut clicked = None;
        for entry in self.list(folder) {
            if entry.is_dir {
                egui::CollapsingHeader::new(format!("📁 {}", entry.name)).id_salt(&entry.path).show(ui, |ui| {
                    if let Some(program) = self.tree(ui, &entry.path, can_run) {
                        clicked = Some(program);
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label(format!("📄 {}", entry.name));
                    if is_runnable(&entry.path) && ui.add_enabled(can_run, egui::Button::new("▶ Запустить")).clicked() {
                        clicked = Some(entry.path.clone());
                    }
                });
            }
        }
        clicked
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        self.poll();
        if self.running.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        let idle = self.running.is_none();

        egui::Panel::top("vault").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.label("Хранилище:");
                ui.add(egui::TextEdit::singleline(&mut self.vault_text).desired_width(360.0));
                if ui.button("Обзор…").clicked() {
                    if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                        self.vault_text = folder.to_string_lossy().into_owned();
                        self.connect();
                    }
                }
                if ui.button("Подключить").clicked() {
                    self.connect();
                }
                if ui.add_enabled(self.vault.is_some(), egui::Button::new("Обновить")).clicked() {
                    self.refresh();
                }
            });
        });

        egui::Panel::bottom("log").resizable(true).default_size(160.0).show(root, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Журнал");
                if let Some((label, _)) = &self.running {
                    ui.spinner();
                    ui.label(format!("выполняется: {label}"));
                }
                if ui.small_button("очистить").clicked() {
                    self.log.clear();
                }
            });
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                for line in &self.log {
                    ui.monospace(line);
                }
            });
        });

        egui::Panel::left("environments").resizable(true).default_size(220.0).show(root, |ui| {
            ui.strong("Окружения");
            ui.separator();
            egui::ScrollArea::vertical().max_height(ui.available_height() - 70.0).show(ui, |ui| {
                for name in self.environments.clone() {
                    if ui.selectable_label(self.selected.as_ref() == Some(&name), &name).clicked() {
                        self.selected = Some(name);
                    }
                }
                if self.vault.is_some() && self.environments.is_empty() {
                    ui.weak("пока нет ни одного");
                }
            });
            ui.separator();
            ui.add_enabled_ui(self.vault.is_some() && idle, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.new_name).hint_text("имя нового окружения"));
                if ui.button("➕ Создать окружение").clicked() {
                    self.create(ctx);
                }
            });
        });

        egui::CentralPanel::default().show(root, |ui| {
            let Some(name) = self.selected.clone() else {
                ui.weak(if self.vault.is_some() { "выберите окружение слева или создайте новое" } else { "укажите папку хранилища" });
                return;
            };
            let Some(folder) = self.environment_folder(&name) else { return };
            ui.heading(&name);
            ui.horizontal(|ui| {
                ui.add_enabled_ui(idle, |ui| {
                    if ui.button("⬇ Pull папки…").clicked() {
                        if let Some(from) = rfd::FileDialog::new().pick_folder() {
                            self.pull(&name, from, ctx);
                            self.tab = Tab::Source;
                        }
                    }
                    if ui.button("⬇ Pull файла…").clicked() {
                        if let Some(from) = rfd::FileDialog::new().pick_file() {
                            self.pull(&name, from, ctx);
                            self.tab = Tab::Source;
                        }
                    }
                });
                if ui.button("Открыть папку окружения").clicked() {
                    open_in_explorer(&folder);
                }
            });
            ui.horizontal(|ui| {
                ui.label("Аргументы запуска:");
                ui.add(egui::TextEdit::singleline(&mut self.args).hint_text("например /VERYSILENT").desired_width(320.0));
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Source, "Папка плагина");
                ui.selectable_value(&mut self.tab, Tab::Drive, "Диск C:");
            });
            let (root, empty) = match self.tab {
                Tab::Source => (folder.join("source"), "пусто: сначала сделайте pull папки плагина"),
                Tab::Drive => (folder.join("C"), "диск C: окружения пока пуст"),
            };
            ui.horizontal(|ui| {
                ui.weak(match self.tab {
                    Tab::Source => "то, что скопировано в окружение командой pull",
                    Tab::Drive => "то, что программы внутри окружения видят как C:\\",
                });
                if ui.small_button("открыть в проводнике").clicked() {
                    open_in_explorer(&root);
                }
            });
            ui.separator();
            let mut to_run = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if self.list(&root).is_empty() {
                    ui.weak(empty);
                } else {
                    to_run = self.tree(ui, &root, idle);
                }
            });
            if let Some(program) = to_run {
                self.run(&name, &program, ctx);
            }
        });
    }
}

fn main() -> eframe::Result {
    if std::env::var_os("WGPU_BACKEND").is_none() {
        unsafe { std::env::set_var("WGPU_BACKEND", "dx12") };
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 640.0]).with_title("PluginVault"),
        ..Default::default()
    };
    eframe::run_native("PluginVault", options, Box::new(|_| Ok(Box::new(App::new()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_arguments_keep_quoted_parts_together() {
        assert_eq!(split_args(r#"/VERYSILENT /DIR="C:\Program Files\X"  -q"#), ["/VERYSILENT", r"/DIR=C:\Program Files\X", "-q"]);
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn only_programs_and_scripts_can_be_run() {
        assert!(is_runnable(Path::new("Setup.EXE")));
        assert!(is_runnable(Path::new("install.bat")) && needs_console(Path::new("install.bat")));
        assert!(!is_runnable(Path::new("readme.txt")));
        assert!(!needs_console(Path::new("setup.exe")));
    }
}
