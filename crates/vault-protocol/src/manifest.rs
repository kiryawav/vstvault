pub const FILE_NAME: &str = "vault-manifest.txt";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub host: String,
    pub plugin: String,
    pub environment: String,
    pub factory_vendor: String,
    pub factory_url: String,
    pub factory_email: String,
    pub factory_flags: i32,
    pub cid: [u8; 16],
    pub cardinality: i32,
    pub category: String,
    pub name: String,
    pub class_flags: u32,
    pub sub_categories: String,
    pub vendor: String,
    pub version: String,
    pub sdk_version: String,
}

impl Manifest {
    pub fn to_text(&self) -> Result<String, String> {
        let hex: String = self.cid.iter().map(|b| format!("{b:02X}")).collect();
        let fields = [
            ("host", self.host.as_str()),
            ("plugin", self.plugin.as_str()),
            ("environment", self.environment.as_str()),
            ("factory_vendor", self.factory_vendor.as_str()),
            ("factory_url", self.factory_url.as_str()),
            ("factory_email", self.factory_email.as_str()),
            ("factory_flags", &self.factory_flags.to_string()),
            ("cid", &hex),
            ("cardinality", &self.cardinality.to_string()),
            ("category", self.category.as_str()),
            ("name", self.name.as_str()),
            ("class_flags", &self.class_flags.to_string()),
            ("sub_categories", self.sub_categories.as_str()),
            ("vendor", self.vendor.as_str()),
            ("version", self.version.as_str()),
            ("sdk_version", self.sdk_version.as_str()),
        ];
        let mut text = String::new();
        for (key, value) in fields {
            if value.contains(['\r', '\n']) {
                return Err(format!("manifest value for {key} contains a line break"));
            }
            text.push_str(key);
            text.push('=');
            text.push_str(value);
            text.push('\n');
        }
        Ok(text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut manifest = Self::default();
        let mut has_cid = false;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let (key, value) = line.split_once('=').ok_or_else(|| format!("bad manifest line: {line}"))?;
            let number = || format!("bad number for {key}: {value}");
            match key {
                "host" => manifest.host = value.into(),
                "plugin" => manifest.plugin = value.into(),
                "environment" => manifest.environment = value.into(),
                "factory_vendor" => manifest.factory_vendor = value.into(),
                "factory_url" => manifest.factory_url = value.into(),
                "factory_email" => manifest.factory_email = value.into(),
                "factory_flags" => manifest.factory_flags = value.parse().map_err(|_| number())?,
                "cid" => {
                    manifest.cid = parse_cid(value)?;
                    has_cid = true;
                }
                "cardinality" => manifest.cardinality = value.parse().map_err(|_| number())?,
                "category" => manifest.category = value.into(),
                "name" => manifest.name = value.into(),
                "class_flags" => manifest.class_flags = value.parse().map_err(|_| number())?,
                "sub_categories" => manifest.sub_categories = value.into(),
                "vendor" => manifest.vendor = value.into(),
                "version" => manifest.version = value.into(),
                "sdk_version" => manifest.sdk_version = value.into(),
                _ => return Err(format!("unknown manifest key: {key}")),
            }
        }
        if !has_cid || manifest.host.is_empty() || manifest.plugin.is_empty() {
            return Err("manifest lacks cid, host or plugin".into());
        }
        Ok(manifest)
    }
}

fn parse_cid(hex: &str) -> Result<[u8; 16], String> {
    let bad = || format!("bad cid: {hex}");
    if hex.len() != 32 || !hex.is_ascii() {
        return Err(bad());
    }
    let mut cid = [0; 16];
    for (index, byte) in cid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).map_err(|_| bad())?;
    }
    Ok(cid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            host: "D:\\vault\\vault-host.exe".into(),
            plugin: "C:\\Program Files\\Common Files\\VST3\\OTT.vst3".into(),
            environment: "D:\\vault\\plugins\\OTT".into(),
            factory_vendor: "Xfer Records".into(),
            factory_url: "https://xferrecords.com".into(),
            factory_flags: 16,
            cid: [0x58, 0x54, 0x53, 0x56, 0x54, 0x66, 0x6F, 0x54, 0x74, 0x74, 0, 0, 0, 0, 0, 0xFF],
            cardinality: i32::MAX,
            category: "Audio Module Class".into(),
            name: "A=B Плагин".into(),
            sub_categories: "Fx|Dynamics".into(),
            version: "1,3,7,0".into(),
            ..Manifest::default()
        }
    }

    #[test]
    fn survives_a_round_trip_including_equals_signs_and_unicode() {
        let manifest = sample();
        assert_eq!(Manifest::parse(&manifest.to_text().unwrap()).unwrap(), manifest);
    }

    #[test]
    fn identity_bytes_are_exact() {
        let text = sample().to_text().unwrap();
        assert!(text.contains("cid=5854535654666F54747400000000" ));
        assert_eq!(Manifest::parse(&text).unwrap().cid, sample().cid);
    }

    #[test]
    fn line_breaks_in_values_are_refused() {
        let manifest = Manifest { name: "two\nlines".into(), ..sample() };
        assert!(manifest.to_text().is_err());
    }

    #[test]
    fn damaged_manifests_are_errors_not_panics() {
        for text in ["", "no equals sign", "cid=XYZ", "cid=12", "unknown=1", "host=a\nplugin=b", "cid=é"] {
            assert!(Manifest::parse(text).is_err(), "{text:?} must be refused");
        }
    }
}
