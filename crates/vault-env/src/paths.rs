const PROFILE_IN_ENV: &str = "Users\\current";

#[derive(Clone, Debug, Default)]
pub struct Paths {
    pub overlay: String,
    pub native_overlay: String,
    pub environment: String,
    pub native_environment: String,
    pub c_device: String,
    pub profile: String,
    pub earlier_profiles: Vec<String>,
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

fn under<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let tail = strip_prefix_ignore_case(path, root)?;
    if tail.is_empty() {
        return Some("");
    }
    tail.strip_prefix('\\')
}

impl Paths {
    pub fn virtual_of(&self, nt: &str) -> Option<String> {
        let path = nt.trim_end_matches('\\');
        let dos = ["\\??\\", "\\DosDevices\\", "\\GLOBAL??\\"].iter().find_map(|prefix| strip_prefix_ignore_case(path, prefix));
        let inside = match dos {
            Some(dos) if under(dos, &self.overlay).is_some() => return under(dos, &self.overlay).map(str::to_owned),
            Some(dos) if !self.environment.is_empty() && under(dos, &self.environment).is_some() => return None,
            Some(dos) => under(dos, "C:")?,
            None => {
                if let Some(tail) = under(path, &self.native_overlay) {
                    return Some(tail.to_owned());
                }
                if !self.native_environment.is_empty() && under(path, &self.native_environment).is_some() {
                    return None;
                }
                under(path, &self.c_device)?
            }
        };
        if inside.starts_with(['$', ':']) || inside.contains(':') {
            return None;
        }
        for profile in std::iter::once(&self.profile).chain(&self.earlier_profiles).filter(|profile| !profile.is_empty()) {
            if let Some(tail) = under(inside, profile) {
                return Some(if tail.is_empty() { PROFILE_IN_ENV.to_owned() } else { format!("{PROFILE_IN_ENV}\\{tail}") });
            }
        }
        Some(inside.to_owned())
    }

    fn real_relative(&self, virtual_path: &str) -> String {
        match under(virtual_path, PROFILE_IN_ENV) {
            Some("") => self.profile.clone(),
            Some(tail) => format!("{}\\{tail}", self.profile),
            None => virtual_path.to_owned(),
        }
    }

    pub fn overlay_win32(&self, virtual_path: &str) -> String {
        if virtual_path.is_empty() { self.overlay.clone() } else { format!("{}\\{virtual_path}", self.overlay) }
    }

    pub fn real_win32(&self, virtual_path: &str) -> String {
        format!("C:\\{}", self.real_relative(virtual_path))
    }

    pub fn overlay_nt(&self, virtual_path: &str) -> String {
        format!("\\??\\{}", self.overlay_win32(virtual_path))
    }

    pub fn real_nt(&self, virtual_path: &str) -> String {
        format!("\\??\\{}", self.real_win32(virtual_path))
    }
}

pub fn parent(virtual_path: &str) -> Option<&str> {
    if virtual_path.is_empty() {
        return None;
    }
    Some(virtual_path.rsplit_once('\\').map_or("", |(parent, _)| parent))
}

pub fn native_name(win32: &str, device_of: &impl Fn(&str) -> Option<String>) -> Option<String> {
    let drive = win32.get(..2)?;
    let tail = &win32[2..];
    let device = device_of(drive)?;
    match device.strip_prefix("\\??\\") {
        Some(target) => Some(format!("{}{tail}", native_name(target, device_of)?)),
        None => Some(format!("{device}{tail}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths {
            overlay: r"D:\Vault\Plugin\C".into(),
            native_overlay: r"\Device\HarddiskVolume7\Vault\Plugin\C".into(),
            environment: r"D:\Vault\Plugin".into(),
            native_environment: r"\Device\HarddiskVolume7\Vault\Plugin".into(),
            c_device: r"\Device\HarddiskVolume3".into(),
            profile: r"Users\kyats".into(),
            earlier_profiles: vec![r"Users\anna".into()],
        }
    }

    #[test]
    fn the_ways_to_name_c_all_mean_the_same_file() {
        for nt in [
            r"\??\C:\Program Files\Plugin.vst3",
            r"\??\c:\Program Files\Plugin.vst3",
            r"\DosDevices\C:\Program Files\Plugin.vst3",
            r"\GLOBAL??\C:\Program Files\Plugin.vst3",
            r"\Device\HarddiskVolume3\Program Files\Plugin.vst3",
            r"\??\C:\Program Files\Plugin.vst3\",
        ] {
            assert_eq!(paths().virtual_of(nt).as_deref(), Some(r"Program Files\Plugin.vst3"), "{nt}");
        }
    }

    #[test]
    fn other_drives_and_devices_are_left_alone() {
        for nt in [r"\??\D:\Samples\kick.wav", r"\Device\NamedPipe\vault.1", r"\Device\HarddiskVolume33\x", r"\??\pipe\x", r"\??\C:"] {
            let result = paths().virtual_of(nt);
            assert!(result.is_none() || nt == r"\??\C:", "{nt}: {result:?}");
        }
    }

    #[test]
    fn a_folder_with_a_similar_name_is_not_inside_the_environment() {
        assert_eq!(paths().virtual_of(r"\??\D:\Vault\Plugin\Cache\x"), None);
        assert_eq!(paths().virtual_of(r"\Device\HarddiskVolume7\Vault\Plugin\Cache\x"), None);
    }

    #[test]
    fn a_path_already_in_the_environment_keeps_its_meaning() {
        for nt in [r"\??\D:\Vault\Plugin\C\Program Files\x", r"\Device\HarddiskVolume7\Vault\Plugin\C\Program Files\x"] {
            assert_eq!(paths().virtual_of(nt).as_deref(), Some(r"Program Files\x"), "{nt}");
        }
    }

    #[test]
    fn an_environment_on_c_itself_is_not_taken_for_ordinary_files_on_c() {
        let p = Paths {
            overlay: r"C:\Vault\Plugin\C".into(),
            native_overlay: r"\Device\HarddiskVolume3\Vault\Plugin\C".into(),
            environment: r"C:\Vault\Plugin".into(),
            native_environment: r"\Device\HarddiskVolume3\Vault\Plugin".into(),
            ..paths()
        };
        for nt in [r"\??\C:\Vault\Plugin\C\Program Files\x", r"\Device\HarddiskVolume3\Vault\Plugin\C\Program Files\x"] {
            assert_eq!(p.virtual_of(nt).as_deref(), Some(r"Program Files\x"), "{nt}");
        }
        for nt in [r"\??\C:\Vault\Plugin\temp\is-1.tmp", r"\Device\HarddiskVolume3\Vault\Plugin\source\setup.exe", r"\??\C:\Vault\Plugin"] {
            assert_eq!(p.virtual_of(nt), None, "{nt}");
        }
        assert_eq!(p.virtual_of(r"\??\C:\Vault\PluginOther\x").as_deref(), Some(r"Vault\PluginOther\x"));
    }

    #[test]
    fn the_user_name_does_not_get_into_the_environment() {
        let p = paths();
        let v = p.virtual_of(r"\??\C:\Users\kyats\AppData\Roaming\Vendor\a.txt").unwrap();
        assert_eq!(v, r"Users\current\AppData\Roaming\Vendor\a.txt");
        assert_eq!(p.overlay_win32(&v), r"D:\Vault\Plugin\C\Users\current\AppData\Roaming\Vendor\a.txt");
        assert_eq!(p.real_win32(&v), r"C:\Users\kyats\AppData\Roaming\Vendor\a.txt");
        assert_eq!(p.virtual_of(r"\??\C:\Users\kyats").as_deref(), Some(r"Users\current"));
    }

    #[test]
    fn a_path_stored_under_another_user_leads_to_the_same_files() {
        let p = paths();
        let stored = p.virtual_of(r"\??\C:\Users\anna\AppData\Roaming\Vendor\a.txt").unwrap();
        assert_eq!(stored, r"Users\current\AppData\Roaming\Vendor\a.txt");
        assert_eq!(p.real_win32(&stored), r"C:\Users\kyats\AppData\Roaming\Vendor\a.txt");
        assert_eq!(p.virtual_of(r"\??\C:\Users\annabel\x").as_deref(), Some(r"Users\annabel\x"));
    }

    #[test]
    fn streams_and_volume_files_are_not_redirected() {
        assert_eq!(paths().virtual_of(r"\??\C:\Windows\file.txt:Zone.Identifier"), None);
        assert_eq!(paths().virtual_of(r"\??\C:\$Extend\$Quota"), None);
    }

    #[test]
    fn parents_end_at_the_root_of_the_disk() {
        assert_eq!(parent(r"Program Files\Vendor"), Some("Program Files"));
        assert_eq!(parent("Program Files"), Some(""));
        assert_eq!(parent(""), None);
    }

    #[test]
    fn a_letter_standing_for_a_folder_resolves_to_the_real_drive() {
        let devices = |drive: &str| match drive {
            "D:" => Some(r"\Device\HarddiskVolume7".to_owned()),
            "X:" => Some(r"\??\D:\Vault".to_owned()),
            _ => None,
        };
        assert_eq!(native_name(r"D:\Vault\P\C", &devices).as_deref(), Some(r"\Device\HarddiskVolume7\Vault\P\C"));
        assert_eq!(native_name(r"X:\P\C", &devices).as_deref(), Some(r"\Device\HarddiskVolume7\Vault\P\C"));
        assert_eq!(native_name(r"Q:\P", &devices), None);
    }
}
