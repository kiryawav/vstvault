use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PLUGINS_FOLDER: &str = "plugins";
const CONNECTIONS_FOLDER: &str = "connections";
const OVERLAY_FOLDER: &str = "C";
const IDENTITY_FOLDER: &str = "identity";
const SOURCE_FOLDER: &str = "source";
const PROFILE_IN_ENV: &str = "Users\\current";
const MARK_FILE: &str = "environment.txt";

pub fn plain_path(path: &Path) -> Result<PathBuf, String> {
    let full = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if full.exists() { Ok(full) } else { Err(format!("{} does not exist", full.display())) }
}

fn check_name(name: &str) -> Result<(), String> {
    let bad = name.is_empty() || name.ends_with([' ', '.']) || name.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c));
    if bad { Err(format!("bad plugin name {name:?}: it has to work as a folder name")) } else { Ok(()) }
}

fn make_writable(folder: &Path) {
    let Ok(entries) = fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            let mut permissions = metadata.permissions();
            if permissions.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = fs::set_permissions(&path, permissions);
            }
            if metadata.is_dir() {
                make_writable(&path);
            }
        }
    }
}

pub struct Environment {
    pub name: String,
    pub root: PathBuf,
}

impl Environment {
    pub fn run(&self, vault_run: &Path, program: &Path, args: &[String]) -> Result<i32, String> {
        let mut command = Command::new(vault_run);
        command.arg("--env").arg(&self.root).arg(program).args(args);
        if let Some(folder) = program.parent().filter(|folder| folder.is_dir()) {
            command.current_dir(folder);
        }
        let status = command.status().map_err(|e| format!("cannot start {}: {e}", vault_run.display()))?;
        status.code().ok_or_else(|| "the program ended without an exit code".to_string())
    }

    pub fn pull(&self, from: &Path) -> Result<PathBuf, String> {
        let name = from.file_name().ok_or_else(|| format!("{} has no name to keep", from.display()))?;
        let to = self.root.join(SOURCE_FOLDER).join(name);
        copy_all(from, &to)?;
        Ok(to)
    }

    pub fn pulled(&self) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(self.root.join(SOURCE_FOLDER)).into_iter().flatten().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    pub fn find_program(&self, program: &Path) -> Result<PathBuf, String> {
        if program.is_absolute() {
            return Ok(program.to_path_buf());
        }
        let source = self.root.join(SOURCE_FOLDER);
        let inside = self.pulled().into_iter().map(|name| source.join(name).join(program));
        std::iter::once(source.join(program)).chain(inside).find(|candidate| candidate.is_file()).ok_or_else(|| {
            format!("{} is not in what was pulled into {}: {}", program.display(), self.name, self.pulled().join(", "))
        })
    }

    pub fn plugins(&self) -> Vec<String> {
        let profile = std::env::var("USERPROFILE").unwrap_or_default();
        find_plugins(&self.root.join(OVERLAY_FOLDER), &profile)
    }

    fn identity_file(&self, plugin: &str) -> PathBuf {
        let stem = Path::new(plugin).file_stem().unwrap_or_default().to_string_lossy().into_owned();
        self.root.join(IDENTITY_FOLDER).join(format!("{stem}.txt"))
    }

    pub fn saved_identity(&self, plugin: &str) -> Option<String> {
        fs::read_to_string(self.identity_file(plugin)).ok()
    }

    pub fn save_identity(&self, plugin: &str, identity: &str) -> Result<(), String> {
        let file = self.identity_file(plugin);
        if let Some(folder) = file.parent() {
            fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
        }
        fs::write(&file, identity).map_err(|e| format!("cannot write {}: {e}", file.display()))
    }

    pub fn clear_temp(&self) {
        let temp = self.root.join(vault_env::TEMP_FOLDER);
        make_writable(&temp);
        let _ = fs::remove_dir_all(temp);
    }

    fn remove(self) -> Result<(), String> {
        make_writable(&self.root);
        fs::remove_dir_all(&self.root).map_err(|e| format!("cannot remove {}, is a DAW using it? {e}", self.root.display()))
    }
}

fn copy_all(from: &Path, to: &Path) -> Result<(), String> {
    let metadata = fs::metadata(from).map_err(|e| format!("{}: {e}", from.display()))?;
    if metadata.is_dir() {
        fs::create_dir_all(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
        for entry in fs::read_dir(from).map_err(|e| format!("cannot read {}: {e}", from.display()))? {
            let entry = entry.map_err(|e| format!("cannot read {}: {e}", from.display()))?;
            copy_all(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if let Some(folder) = to.parent() {
        fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    }
    if to.exists() {
        make_writable(to);
        let mut permissions = fs::metadata(to).map_err(|e| e.to_string())?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        let _ = fs::set_permissions(to, permissions);
    }
    fs::copy(from, to).map(|_| ()).map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))
}

fn find_plugins(overlay: &Path, profile: &str) -> Vec<String> {
    let mut plugins = Vec::new();
    collect(overlay, "C:", profile, &mut plugins);
    plugins.sort();
    plugins
}

fn collect(folder: &Path, virtual_path: &str, profile: &str, plugins: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let inner = format!("{virtual_path}\\{name}");
        let inner = if inner.eq_ignore_ascii_case(&format!("C:\\{PROFILE_IN_ENV}")) && !profile.is_empty() { profile.to_owned() } else { inner };
        if Path::new(&name).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("vst3")) {
            plugins.push(inner);
        } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            collect(&entry.path(), &inner, profile, plugins);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connection {
    pub out: PathBuf,
    pub test_identity: bool,
    pub bridges: Vec<(String, PathBuf)>,
}

impl Connection {
    fn to_text(&self) -> String {
        let mut text = format!("out={}\ntest_identity={}\n", self.out.display(), self.test_identity);
        for (name, bridge) in &self.bridges {
            text.push_str(&format!("bridge={name}\t{}\n", bridge.display()));
        }
        text
    }

    fn parse(text: &str) -> Self {
        let mut connection = Self::default();
        for line in text.lines() {
            if let Some(out) = line.strip_prefix("out=") {
                connection.out = PathBuf::from(out);
            } else if let Some(flag) = line.strip_prefix("test_identity=") {
                connection.test_identity = flag == "true";
            } else if let Some((name, bridge)) = line.strip_prefix("bridge=").and_then(|rest| rest.split_once('\t')) {
                connection.bridges.push((name.to_owned(), PathBuf::from(bridge)));
            }
        }
        connection
    }
}

pub struct Vault {
    pub root: PathBuf,
}

impl Vault {
    pub fn at(root: &Path) -> Result<Self, String> {
        fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
        Ok(Self { root: plain_path(root)? })
    }

    fn plugins_folder(&self) -> PathBuf {
        self.root.join(PLUGINS_FOLDER)
    }

    pub fn create(&self, name: &str) -> Result<Environment, String> {
        check_name(name)?;
        let folder = self.plugins_folder().join(name);
        if folder.exists() {
            return Err(format!("{name} is already installed. Remove it first, or run its installer with the run command"));
        }
        fs::create_dir_all(folder.join(OVERLAY_FOLDER)).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
        fs::write(folder.join(MARK_FILE), "").map_err(|e| format!("cannot write to {}: {e}", folder.display()))?;
        Ok(Environment { name: name.to_owned(), root: plain_path(&folder)? })
    }

    pub fn open(&self, name: &str) -> Result<Environment, String> {
        check_name(name)?;
        let folder = self.plugins_folder().join(name);
        if !folder.join(MARK_FILE).is_file() {
            return Err(format!("{name} is not installed in {}", self.root.display()));
        }
        Ok(Environment { name: name.to_owned(), root: plain_path(&folder)? })
    }

    pub fn environments(&self) -> Vec<Environment> {
        let mut found: Vec<Environment> = fs::read_dir(self.plugins_folder())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().join(MARK_FILE).is_file())
            .filter_map(|entry| self.open(&entry.file_name().to_string_lossy()).ok())
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        found
    }

    fn connection_file(&self) -> PathBuf {
        let computer = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "this-pc".to_owned());
        self.root.join(CONNECTIONS_FOLDER).join(format!("{computer}.txt"))
    }

    pub fn connection(&self) -> Option<Connection> {
        fs::read_to_string(self.connection_file()).ok().map(|text| Connection::parse(&text))
    }

    pub fn save_connection(&self, connection: &Connection) -> Result<(), String> {
        let file = self.connection_file();
        if let Some(folder) = file.parent() {
            fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
        }
        fs::write(&file, connection.to_text()).map_err(|e| format!("cannot write {}: {e}", file.display()))
    }

    pub fn forget_connection(&self) -> Result<(), String> {
        fs::remove_file(self.connection_file()).map_err(|e| format!("cannot update {}: {e}", self.root.display()))
    }

    pub fn remove(&self, name: &str) -> Result<(), String> {
        let environment = self.open(name)?;
        if let Some(mut connection) = self.connection() {
            for (_, bridge) in connection.bridges.iter().filter(|(owner, _)| owner == name) {
                remove_bridge(bridge)?;
            }
            connection.bridges.retain(|(owner, _)| owner != name);
            self.save_connection(&connection)?;
        }
        environment.remove()
    }
}

pub fn remove_bridge(bridge: &Path) -> Result<(), String> {
    if !bridge.exists() {
        return Ok(());
    }
    make_writable(bridge);
    fs::remove_dir_all(bridge).map_err(|e| format!("cannot remove {}, is a DAW using it? {e}", bridge.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("vault-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn touch(root: &Path, file: &str) {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn plugins_are_named_as_seen_from_inside() {
        let root = folder("paths");
        touch(&root, r"Program Files\Common Files\VST3\Vendor\Single.VST3");
        touch(&root, r"Program Files\Common Files\VST3\Bundle.vst3\Contents\x86_64-win\Bundle.vst3");
        touch(&root, r"Users\current\AppData\Personal.vst3");
        touch(&root, r"ProgramData\Vendor\readme.txt");
        let found = find_plugins(&root, r"C:\Users\me");
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(found, [r"C:\Program Files\Common Files\VST3\Bundle.vst3", r"C:\Program Files\Common Files\VST3\Vendor\Single.VST3", r"C:\Users\me\AppData\Personal.vst3"]);
    }

    #[test]
    fn a_name_cannot_lead_out_of_the_vault() {
        let root = folder("names");
        let vault = Vault::at(&root).unwrap();
        for name in ["", ".", "..", r"..\other", "a/b", r"C:\x", "tail.", "tail "] {
            assert!(vault.create(name).is_err(), "{name:?}");
            assert!(vault.open(name).is_err(), "{name:?}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_new_environment_is_listed_and_a_second_with_its_name_refused() {
        let root = folder("create");
        let vault = Vault::at(&root).unwrap();
        let made = vault.create("Плагин 1").unwrap();
        assert!(made.root.join(OVERLAY_FOLDER).is_dir());
        assert!(vault.create("Плагин 1").is_err());
        assert_eq!(vault.environments().iter().map(|e| e.name.clone()).collect::<Vec<_>>(), ["Плагин 1"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_takes_this_pcs_bridge_and_nothing_else_along() {
        let root = folder("remove");
        let outside = folder("remove-outside");
        let vault = Vault::at(&root).unwrap();
        let bridge = outside.join("Plugin.vst3");
        let other_bridge = outside.join("Other.vst3");
        touch(&bridge, r"Contents\x86_64-win\Plugin.vst3");
        touch(&other_bridge, r"Contents\x86_64-win\Other.vst3");
        touch(&outside, "keep.txt");
        let environment = vault.create("Plugin").unwrap();
        vault.create("Other").unwrap();
        touch(&environment.root, r"C\Program Files\file.dll");
        touch(&environment.root, r"C\Users\current\Documents\preset.txt");
        for read_only in [r"C\Users\current\Documents", r"C\Users\current\Documents\preset.txt"] {
            let path = environment.root.join(read_only);
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&path, permissions).unwrap();
        }
        vault
            .save_connection(&Connection { out: outside.clone(), test_identity: false, bridges: vec![("Plugin".into(), bridge.clone()), ("Other".into(), other_bridge.clone())] })
            .unwrap();
        vault.remove("Plugin").unwrap();
        assert!(!bridge.exists());
        assert!(other_bridge.exists());
        assert!(outside.join("keep.txt").exists());
        assert_eq!(vault.environments().iter().map(|e| e.name.clone()).collect::<Vec<_>>(), ["Other"]);
        assert_eq!(vault.connection().unwrap().bridges, [("Other".to_owned(), other_bridge)]);
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn a_pulled_folder_is_copied_in_and_its_programs_are_found_by_short_paths() {
        let root = folder("pull");
        let outside = folder("pull-outside").join("Plugin Setup");
        touch(&outside, "install.bat");
        touch(&outside, r"bin\setup.exe");
        let environment = Vault::at(&root).unwrap().create("P").unwrap();
        let pulled = environment.pull(&outside).unwrap();
        assert!(pulled.join(r"bin\setup.exe").is_file());
        environment.pull(&outside).unwrap();
        assert_eq!(environment.pulled(), ["Plugin Setup"]);
        assert_eq!(environment.find_program(Path::new("install.bat")).unwrap(), pulled.join("install.bat"));
        assert_eq!(environment.find_program(Path::new(r"Plugin Setup\bin\setup.exe")).unwrap(), pulled.join(r"bin\setup.exe"));
        assert_eq!(environment.find_program(Path::new(r"bin\setup.exe")).unwrap(), pulled.join(r"bin\setup.exe"));
        assert!(environment.find_program(Path::new("missing.exe")).is_err());
        let inside_only = Path::new(r"C:\Program Files\Vendor\activate.exe");
        assert_eq!(environment.find_program(inside_only).unwrap(), inside_only);
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(outside.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_folder_without_the_mark_is_not_an_environment() {
        let root = folder("nomark");
        let vault = Vault::at(&root).unwrap();
        fs::create_dir_all(vault.plugins_folder().join("Other")).unwrap();
        assert!(vault.open("Other").is_err());
        assert!(vault.environments().is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_connection_survives_being_saved_including_odd_names() {
        let connection = Connection {
            out: PathBuf::from(r"C:\Users\Анна\VST3"),
            test_identity: true,
            bridges: vec![("Plug = in".into(), PathBuf::from(r"C:\Users\Анна\VST3\Plug = in.vst3"))],
        };
        assert_eq!(Connection::parse(&connection.to_text()), connection);
        assert_eq!(Connection::parse(""), Connection::default());
    }

    #[test]
    fn a_saved_identity_is_found_by_the_plugin_it_belongs_to() {
        let root = folder("identity");
        let vault = Vault::at(&root).unwrap();
        let environment = vault.create("P").unwrap();
        environment.save_identity(r"C:\Program Files\A.vst3", "a").unwrap();
        assert_eq!(environment.saved_identity(r"C:\Other\A.vst3").as_deref(), Some("a"));
        assert_eq!(environment.saved_identity(r"C:\Program Files\B.vst3"), None);
        fs::remove_dir_all(&root).unwrap();
    }
}
