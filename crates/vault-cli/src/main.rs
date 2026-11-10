mod store;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use store::{Connection, Environment, Vault, plain_path, remove_bridge};
use vault_protocol::HostSession;
use vault_protocol::manifest::{self, Manifest};

const TOOLS_FOLDER: &str = "tools";
const USAGE: &str = "usage (--vault is not needed when running from a vault's tools folder):
  vault-cli connect [--vault DIR] [--out VST3_DIR] [--test-identity]
  vault-cli disconnect [--vault DIR]
  vault-cli create [--vault DIR] --name NAME
  vault-cli pull [--vault DIR] --name NAME FOLDER_OR_FILE
  vault-cli run [--vault DIR] --name NAME PROGRAM [ARGS...]    (.exe, .bat or .cmd; a relative path
                                                               is looked for in what was pulled)
  vault-cli check [--vault DIR]
  vault-cli list [--vault DIR]
  vault-cli remove [--vault DIR] --name NAME
  vault-cli bridge --plugin PATH --out VST3_DIR [--test-identity]";

#[derive(Default)]
struct Options {
    vault: Option<PathBuf>,
    name: Option<String>,
    out: Option<PathBuf>,
    plugin: Option<PathBuf>,
    test_identity: bool,
    rest: Vec<OsString>,
}

fn parse_options(args: impl Iterator<Item = OsString>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.peekable();
    while let Some(flag) = args.next_if(|arg| arg.to_string_lossy().starts_with("--")) {
        match flag.to_str() {
            Some("--vault") => options.vault = args.next().map(PathBuf::from),
            Some("--name") => options.name = args.next().map(|name| name.to_string_lossy().into_owned()),
            Some("--out") => options.out = args.next().map(PathBuf::from),
            Some("--plugin") => options.plugin = args.next().map(PathBuf::from),
            Some("--test-identity") => options.test_identity = true,
            _ => return Err(USAGE.into()),
        }
    }
    options.rest = args.collect();
    Ok(options)
}

fn needed<T>(value: Option<T>) -> Result<T, String> {
    value.ok_or_else(|| USAGE.to_string())
}

fn the_vault(given: Option<PathBuf>) -> Result<Vault, String> {
    if let Some(given) = given {
        return Vault::at(&given);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let tools = exe.parent().filter(|folder| folder.file_name().is_some_and(|name| name.eq_ignore_ascii_case(TOOLS_FOLDER)));
    let vault = tools.and_then(Path::parent).ok_or("this program is not in a vault's tools folder, so give --vault")?;
    Vault::at(vault)
}

const VAULT_LABEL: &str = " [vault]";

fn add_vault_label(manifest: &mut Manifest) {
    if !manifest.name.ends_with(VAULT_LABEL) {
        manifest.name.push_str(VAULT_LABEL);
    }
}

fn apply_test_identity(manifest: &mut Manifest) {
    for byte in &mut manifest.cid[12..] {
        *byte ^= 0x5A;
    }
    add_vault_label(manifest);
}

fn warn_about_limits(manifest: &Manifest, audio_classes: u32) {
    if audio_classes > 1 {
        eprintln!("warning: {} has {audio_classes} audio classes, and only the first is bridged", manifest.name);
    }
}

fn sibling(name: &str) -> Result<PathBuf, String> {
    let path = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name(name);
    if path.is_file() { Ok(path) } else { Err(format!("{} is missing; build the workspace first", path.display())) }
}

fn write_bundle(out: &Path, plugin_path: &str, manifest: &Manifest, test_identity: bool) -> Result<PathBuf, String> {
    let stem = Path::new(plugin_path).file_stem().ok_or("the plugin path has no file name")?.to_string_lossy().into_owned();
    let bundle_name = if test_identity { format!("{stem}-vault.vst3") } else { format!("{stem}.vst3") };
    let bundle = out.join(&bundle_name);
    let dll_dir = bundle.join("Contents").join("x86_64-win");
    let resources = bundle.join("Contents").join("Resources");
    for dir in [&dll_dir, &resources] {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    fs::copy(sibling("vault_bridge.dll")?, dll_dir.join(&bundle_name)).map_err(|e| format!("cannot copy the bridge: {e}"))?;
    let manifest_path = resources.join(manifest::FILE_NAME);
    fs::write(&manifest_path, manifest.to_text()?).map_err(|e| format!("cannot write {}: {e}", manifest_path.display()))?;
    Ok(bundle)
}

fn bridge(options: Options) -> Result<(), String> {
    let plugin_arg = needed(options.plugin)?;
    let out = needed(options.out)?;
    let plugin_text = plain_path(&plugin_arg)?.to_string_lossy().into_owned();
    let (mut manifest, audio_classes) = vault_plugin::read_identity(&plugin_arg)?;
    warn_about_limits(&manifest, audio_classes);
    manifest.plugin.clone_from(&plugin_text);
    manifest.host = sibling("vault-host.exe")?.to_string_lossy().into_owned();
    if options.test_identity {
        apply_test_identity(&mut manifest);
    }
    println!("{}", write_bundle(&out, &plugin_text, &manifest, options.test_identity)?.display());
    Ok(())
}

fn read_and_save_identity(environment: &Environment, host: &Path, plugin: &str) -> Result<Manifest, String> {
    let mut session = HostSession::spawn(host, Some(&environment.root), |_| {})?;
    let (manifest, audio_classes) = session.read_identity(plugin)?;
    session.shutdown();
    warn_about_limits(&manifest, audio_classes);
    environment.save_identity(plugin, &manifest.to_text()?)?;
    Ok(manifest)
}

fn identity_of(environment: &Environment, host: &Path, plugin: &str) -> Result<Manifest, String> {
    match environment.saved_identity(plugin) {
        Some(text) => Manifest::parse(&text),
        None => read_and_save_identity(environment, host, plugin),
    }
}

fn make_bridge(vault: &Vault, environment: &Environment, host: &Path, plugin: &str, connection: &mut Connection) -> Result<PathBuf, String> {
    let mut manifest = identity_of(environment, host, plugin)?;
    manifest.plugin = plugin.to_owned();
    let written = |target: &Path| match relative_to(&connection.out, target).filter(|_| inside(&connection.out, &vault.root) && inside(target, &vault.root)) {
        Some(relative) => relative.to_string_lossy().into_owned(),
        None => target.to_string_lossy().into_owned(),
    };
    manifest.host = written(host);
    manifest.environment = written(&environment.root);
    add_vault_label(&mut manifest);
    if connection.test_identity {
        apply_test_identity(&mut manifest);
    }
    let bundle = write_bundle(&connection.out, plugin, &manifest, connection.test_identity)?;
    if !connection.bridges.iter().any(|(_, known)| known == &bundle) {
        connection.bridges.push((environment.name.clone(), bundle.clone()));
    }
    Ok(bundle)
}

fn inside(path: &Path, folder: &Path) -> bool {
    let mut path = path.components();
    folder.components().all(|part| path.next().is_some_and(|other| part.as_os_str().eq_ignore_ascii_case(other.as_os_str())))
}

fn relative_to(from: &Path, to: &Path) -> Option<PathBuf> {
    let (from, to): (Vec<_>, Vec<_>) = (from.components().collect(), to.components().collect());
    if from.first()?.as_os_str().to_ascii_lowercase() != to.first()?.as_os_str().to_ascii_lowercase() {
        return None;
    }
    let common = from.iter().zip(&to).take_while(|(a, b)| a.as_os_str().eq_ignore_ascii_case(b.as_os_str())).count();
    let mut relative = PathBuf::new();
    for _ in common..from.len() {
        relative.push("..");
    }
    for part in &to[common..] {
        relative.push(part);
    }
    Some(relative)
}

fn bridges_folder(vault: &Vault) -> PathBuf {
    vault.root.join("bridges")
}

fn connect(options: Options) -> Result<(), String> {
    let vault = the_vault(options.vault)?;
    let host = sibling("vault-host.exe")?;
    let earlier = vault.connection();
    let out = match (options.out, &earlier) {
        (Some(out), _) => out,
        (None, Some(earlier)) if !earlier.out.as_os_str().is_empty() => earlier.out.clone(),
        (None, _) => bridges_folder(&vault),
    };
    fs::create_dir_all(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let test_identity = options.test_identity || earlier.as_ref().is_some_and(|earlier| earlier.test_identity);
    let mut connection = Connection { out: plain_path(&out)?, test_identity, bridges: Vec::new() };

    let mut failed = 0;
    for (_, bridge) in earlier.iter().flat_map(|earlier| &earlier.bridges) {
        if let Err(message) = remove_bridge(bridge) {
            failed += 1;
            eprintln!("{message}");
        }
    }
    for environment in vault.environments() {
        environment.clear_temp();
        for plugin in environment.plugins() {
            match make_bridge(&vault, &environment, &host, &plugin, &mut connection) {
                Ok(bundle) => println!("{} -> {}", environment.name, bundle.display()),
                Err(message) => {
                    failed += 1;
                    eprintln!("{}: {message}", environment.name);
                }
            }
        }
    }
    vault.save_connection(&connection)?;
    println!("connected {} to {}", vault.root.display(), connection.out.display());
    if failed == 0 { Ok(()) } else { Err(format!("{failed} problems, see above")) }
}

fn disconnect(options: Options) -> Result<(), String> {
    let vault = the_vault(options.vault)?;
    let connection = vault.connection().ok_or("this PC is not connected")?;
    for (_, bridge) in &connection.bridges {
        remove_bridge(bridge)?;
    }
    for environment in vault.environments() {
        environment.clear_temp();
    }
    vault.forget_connection()?;
    println!("removed {} bridges from {}", connection.bridges.len(), connection.out.display());
    Ok(())
}

fn check(options: Options) -> Result<(), String> {
    let vault = the_vault(options.vault)?;
    let host = sibling("vault-host.exe")?;
    let mut failed = 0;
    for environment in vault.environments() {
        for plugin in environment.plugins() {
            let loaded = HostSession::spawn(&host, Some(&environment.root), |_| {}).and_then(|mut session| {
                let result = session.load(Some(&plugin)).map(|_| ());
                session.shutdown();
                result
            });
            match loaded {
                Ok(()) => println!("{}: loads", environment.name),
                Err(message) => {
                    failed += 1;
                    println!("{}: does not load: {message}", environment.name);
                }
            }
        }
    }
    if failed == 0 { Ok(()) } else { Err(format!("{failed} plugins do not load")) }
}

fn program_and_args(rest: Vec<OsString>) -> Result<(PathBuf, Vec<String>), String> {
    let mut rest = rest.into_iter();
    let program = PathBuf::from(needed(rest.next())?);
    Ok((program, rest.map(|arg| arg.to_string_lossy().into_owned()).collect()))
}

fn create(options: Options) -> Result<(), String> {
    let name = needed(options.name)?;
    let environment = the_vault(options.vault)?.create(&name)?;
    println!("created {}", environment.root.display());
    println!("next: vault-cli pull --name \"{name}\" FOLDER, then vault-cli run --name \"{name}\" INSTALLER");
    Ok(())
}

fn pull(options: Options) -> Result<(), String> {
    let name = needed(options.name)?;
    let environment = the_vault(options.vault)?.open(&name)?;
    let from = plain_path(Path::new(&needed(options.rest.into_iter().next())?))?;
    let to = environment.pull(&from)?;
    println!("copied {} to {}", from.display(), to.display());
    Ok(())
}

fn run_and_add(vault: &Vault, environment: &Environment, program: &Path, args: &[String]) -> Result<i32, String> {
    let code = environment.run(&sibling("vault-run.exe")?, program, args)?;
    eprintln!("{} exited with code {code}", program.file_name().unwrap_or_default().to_string_lossy());
    environment.clear_temp();
    add_new_plugins(vault, environment)?;
    Ok(code)
}

fn add_new_plugins(vault: &Vault, environment: &Environment) -> Result<(), String> {
    let host = sibling("vault-host.exe")?;
    let plugins: Vec<String> = environment.plugins().into_iter().filter(|plugin| environment.saved_identity(plugin).is_none()).collect();
    if plugins.is_empty() {
        return Ok(());
    }
    let mut connection = vault.connection().unwrap_or_else(|| Connection { out: bridges_folder(vault), ..Connection::default() });
    let mut failed = 0;
    for plugin in &plugins {
        let result = read_and_save_identity(environment, &host, plugin)
            .and_then(|_| make_bridge(vault, environment, &host, plugin, &mut connection))
            .map(|bundle| format!("{plugin} -> {}", bundle.display()));
        match result {
            Ok(line) => println!("{line}"),
            Err(message) => {
                failed += 1;
                eprintln!("{plugin}: {message}");
            }
        }
    }
    vault.save_connection(&connection)?;
    if failed == 0 { Ok(()) } else { Err(format!("{failed} of {} plugins failed", plugins.len())) }
}

fn run_program(options: Options) -> Result<(), String> {
    let vault = the_vault(options.vault)?;
    let environment = vault.open(&needed(options.name)?)?;
    let (program, args) = program_and_args(options.rest)?;
    let program = environment.find_program(&program)?;
    let code = run_and_add(&vault, &environment, &program, &args)?;
    if environment.plugins().is_empty() {
        println!("there is no VST3 plugin in {} yet. An installer may have skipped the VST3 format, or wants another step", environment.name);
    }
    if code == 0 { Ok(()) } else { Err(format!("the program exited with code {code}")) }
}

fn list(options: Options) -> Result<(), String> {
    let vault = the_vault(options.vault)?;
    let connection = vault.connection();
    println!("vault: {}", vault.root.display());
    match &connection {
        Some(connection) => println!("this PC is connected, bridges in {}", connection.out.display()),
        None => println!("this PC is not connected"),
    }
    for environment in vault.environments() {
        println!("{}", environment.name);
        for pulled in environment.pulled() {
            println!("  pulled: {pulled}");
        }
        for plugin in environment.plugins() {
            println!("  plugin: {plugin}");
        }
        for (_, bridge) in connection.iter().flat_map(|connection| &connection.bridges).filter(|(owner, _)| owner == &environment.name) {
            println!("  bridge: {}", bridge.display());
        }
    }
    Ok(())
}

fn remove(options: Options) -> Result<(), String> {
    let name = needed(options.name)?;
    the_vault(options.vault)?.remove(&name)?;
    println!("{name} is removed, with its bridge on this PC");
    Ok(())
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let command = args.next().ok_or(USAGE)?;
    let options = parse_options(args)?;
    match command.to_str() {
        Some("connect") => connect(options),
        Some("disconnect") => disconnect(options),
        Some("create") => create(options),
        Some("pull") => pull(options),
        Some("run") => run_program(options),
        Some("check") => check(options),
        Some("list") => list(options),
        Some("remove") => remove(options),
        Some("bridge") => bridge(options),
        _ => Err(USAGE.into()),
    }
}

fn main() -> ! {
    match run() {
        Ok(()) => vault_plugin::end_process(0),
        Err(message) => {
            eprintln!("vault-cli: {message}");
            vault_plugin::end_process(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Options {
        parse_options(args.iter().map(OsString::from)).ok().expect("valid options")
    }

    #[test]
    fn a_programs_own_flags_are_left_for_the_program() {
        let options = parse(&["--vault", "D:\\V", "--name", "P", "setup.exe", "/VERYSILENT", "--also-its-own"]);
        assert_eq!(options.vault, Some(PathBuf::from("D:\\V")));
        assert_eq!(options.name.as_deref(), Some("P"));
        assert_eq!(options.rest, ["setup.exe", "/VERYSILENT", "--also-its-own"]);
    }

    #[test]
    fn unknown_flags_are_refused() {
        assert!(parse_options(["--nonsense"].iter().map(OsString::from)).is_err());
    }

    #[test]
    fn a_bridge_in_the_vault_points_at_the_vault_relatively() {
        let (vault, bridges) = (Path::new(r"D:\PluginVault"), Path::new(r"D:\PluginVault\bridges"));
        assert_eq!(relative_to(bridges, Path::new(r"D:\PluginVault\tools\vault-host.exe")), Some(PathBuf::from(r"..\tools\vault-host.exe")));
        assert_eq!(relative_to(bridges, Path::new(r"d:\pluginvault\plugins\Vital")), Some(PathBuf::from(r"..\plugins\Vital")));
        assert_eq!(relative_to(bridges, Path::new(r"C:\Other")), None);
        assert!(inside(bridges, vault) && inside(Path::new(r"d:\pluginvault\Tools"), vault));
        assert!(!inside(Path::new(r"D:\PluginVaultOld\bridges"), vault));
    }

    #[test]
    fn the_vault_label_is_added_once_and_leaves_the_id_alone() {
        let mut manifest = Manifest { name: "Plugin".into(), cid: [1; 16], ..Manifest::default() };
        add_vault_label(&mut manifest);
        apply_test_identity(&mut manifest);
        assert_eq!(manifest.name, "Plugin [vault]");
        let mut plain = Manifest { name: "Plugin".into(), cid: [1; 16], ..Manifest::default() };
        add_vault_label(&mut plain);
        assert_eq!(plain.cid, [1; 16]);
    }

    #[test]
    fn test_identity_changes_the_id_and_name_only() {
        let mut manifest = Manifest { name: "Plugin".into(), cid: [1; 16], vendor: "V".into(), ..Manifest::default() };
        apply_test_identity(&mut manifest);
        assert_eq!(manifest.name, "Plugin [vault]");
        assert_eq!(manifest.cid[..12], [1; 12]);
        assert_eq!(manifest.cid[12..], [1 ^ 0x5A; 4]);
        assert_eq!(manifest.vendor, "V");
    }
}
