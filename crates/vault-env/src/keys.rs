const CURRENT_USER: &str = "USER\\CURRENT";
const CURRENT_CLASSES: &str = "USER\\CURRENT_CLASSES";
const SOFTWARE: &str = "MACHINE\\SOFTWARE";
const REDIRECTED_NODE: &str = "WOW6432Node";
const KEY_WOW64_64KEY: u32 = 0x100;
const KEY_WOW64_32KEY: u32 = 0x200;

#[derive(Clone, Debug, Default)]
pub struct KeyPaths {
    pub user_sid: String,
    pub hive_root: String,
    pub is_32_bit: bool,
}

fn after<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let head = path.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let tail = &path[prefix.len()..];
    if tail.is_empty() {
        return Some("");
    }
    tail.strip_prefix('\\')
}

fn join(base: &str, tail: &str) -> String {
    if tail.is_empty() { base.to_owned() } else { format!("{base}\\{tail}") }
}

impl KeyPaths {
    pub fn virtual_of(&self, name: &str) -> Option<String> {
        let name = name.trim_end_matches('\\');
        if !self.hive_root.is_empty() {
            if let Some(tail) = after(name, &self.hive_root) {
                return Some(tail.to_owned());
            }
        }
        if let Some(tail) = after(name, "\\REGISTRY\\MACHINE") {
            return Some(join("MACHINE", tail));
        }
        let classes = format!("\\REGISTRY\\USER\\{}_Classes", self.user_sid);
        if let Some(tail) = after(name, &classes) {
            return Some(join(CURRENT_CLASSES, tail));
        }
        let user = format!("\\REGISTRY\\USER\\{}", self.user_sid);
        after(name, &user).map(|tail| join(CURRENT_USER, tail))
    }

    pub fn real_name(&self, virtual_path: &str) -> String {
        if let Some(tail) = after(virtual_path, CURRENT_CLASSES) {
            return join(&format!("\\REGISTRY\\USER\\{}_Classes", self.user_sid), tail);
        }
        if let Some(tail) = after(virtual_path, CURRENT_USER) {
            return join(&format!("\\REGISTRY\\USER\\{}", self.user_sid), tail);
        }
        format!("\\REGISTRY\\{virtual_path}")
    }

    pub fn with_view(&self, virtual_path: String, access: u32) -> String {
        let use_32_bit_view = if self.is_32_bit { access & KEY_WOW64_64KEY == 0 } else { access & KEY_WOW64_32KEY != 0 };
        let Some(tail) = after(&virtual_path, SOFTWARE) else { return virtual_path };
        if !use_32_bit_view || after(tail, REDIRECTED_NODE).is_some() {
            return virtual_path;
        }
        join(&format!("{SOFTWARE}\\{REDIRECTED_NODE}"), tail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> KeyPaths {
        KeyPaths { user_sid: "S-1-5-21-9".into(), hive_root: r"\REGISTRY\A\{ABC}".into(), is_32_bit: false }
    }

    #[test]
    fn real_keys_map_to_virtual_paths_and_back() {
        let p = paths();
        for (real, expected) in [
            (r"\REGISTRY\MACHINE\SOFTWARE\Vendor", r"MACHINE\SOFTWARE\Vendor"),
            (r"\REGISTRY\USER\S-1-5-21-9\Software\Vendor", r"USER\CURRENT\Software\Vendor"),
            (r"\REGISTRY\USER\S-1-5-21-9_Classes\CLSID", r"USER\CURRENT_CLASSES\CLSID"),
            (r"\REGISTRY\MACHINE", "MACHINE"),
        ] {
            let found = p.virtual_of(real).unwrap();
            assert_eq!(found, expected);
            assert_eq!(p.real_name(&found), real);
        }
    }

    #[test]
    fn the_case_of_a_kernel_name_does_not_matter() {
        assert_eq!(paths().virtual_of(r"\Registry\Machine\Software\X").as_deref(), Some(r"MACHINE\Software\X"));
    }

    #[test]
    fn a_key_inside_the_environment_keeps_its_meaning() {
        assert_eq!(paths().virtual_of(r"\REGISTRY\A\{ABC}\MACHINE\SOFTWARE\Vendor").as_deref(), Some(r"MACHINE\SOFTWARE\Vendor"));
        assert_eq!(paths().virtual_of(r"\REGISTRY\A\{ABC}").as_deref(), Some(""));
    }

    #[test]
    fn other_users_and_similar_names_are_left_alone() {
        for name in [r"\REGISTRY\USER\.DEFAULT\Software", r"\REGISTRY\USER\S-1-5-21-90\Software", r"\REGISTRY\MACHINEX", r"\REGISTRY\A\{ABCD}\x", r"\Device\Foo"] {
            assert_eq!(paths().virtual_of(name), None, "{name}");
        }
    }

    #[test]
    fn a_64_bit_program_sees_the_plain_branch_unless_it_asks_for_32() {
        let p = paths();
        assert_eq!(p.with_view(r"MACHINE\SOFTWARE\V".into(), 0), r"MACHINE\SOFTWARE\V");
        assert_eq!(p.with_view(r"MACHINE\SOFTWARE\V".into(), KEY_WOW64_32KEY), r"MACHINE\SOFTWARE\WOW6432Node\V");
    }

    #[test]
    fn a_32_bit_program_sees_the_separate_branch_unless_it_asks_for_64() {
        let p = KeyPaths { is_32_bit: true, ..paths() };
        assert_eq!(p.with_view(r"MACHINE\SOFTWARE\V".into(), 0), r"MACHINE\SOFTWARE\WOW6432Node\V");
        assert_eq!(p.with_view(r"MACHINE\SOFTWARE\V".into(), KEY_WOW64_64KEY), r"MACHINE\SOFTWARE\V");
        assert_eq!(p.with_view(r"MACHINE\SOFTWARE\WOW6432Node\V".into(), 0), r"MACHINE\SOFTWARE\WOW6432Node\V");
        assert_eq!(p.with_view(r"MACHINE\SOFTWAREX".into(), 0), r"MACHINE\SOFTWAREX");
        assert_eq!(p.with_view(r"USER\CURRENT\Software\V".into(), 0), r"USER\CURRENT\Software\V");
    }
}
