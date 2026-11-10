use std::ffi::c_char;
use std::path::Path;

use vault_protocol::manifest::Manifest;
use vst3::Steinberg::*;

use crate::module::Module;

const AUDIO_MODULE_CLASS: &str = "Audio Module Class";

fn text(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn read_identity(path: &Path) -> Result<(Manifest, u32), String> {
    let module = Module::load(path)?;
    let factory = module.factory();
    let mut manifest = Manifest::default();
    let mut audio_classes = 0;
    unsafe {
        let mut info: PFactoryInfo = std::mem::zeroed();
        if factory.getFactoryInfo(&mut info) == kResultOk {
            manifest.factory_vendor = text(&info.vendor);
            manifest.factory_url = text(&info.url);
            manifest.factory_email = text(&info.email);
            manifest.factory_flags = info.flags;
        }
        let factory2 = factory.cast::<IPluginFactory2>();
        for index in 0..factory.countClasses() {
            let mut class: PClassInfo = std::mem::zeroed();
            if factory.getClassInfo(index, &mut class) != kResultOk || text(&class.category) != AUDIO_MODULE_CLASS {
                continue;
            }
            audio_classes += 1;
            if audio_classes > 1 {
                continue;
            }
            manifest.cid = class.cid.map(|b| b as u8);
            manifest.cardinality = class.cardinality;
            manifest.category = text(&class.category);
            manifest.name = text(&class.name);
            if let Some(factory2) = &factory2 {
                let mut extra: PClassInfo2 = std::mem::zeroed();
                if factory2.getClassInfo2(index, &mut extra) == kResultOk {
                    manifest.class_flags = extra.classFlags;
                    manifest.sub_categories = text(&extra.subCategories);
                    manifest.vendor = text(&extra.vendor);
                    manifest.version = text(&extra.version);
                    manifest.sdk_version = text(&extra.sdkVersion);
                }
            }
        }
    }
    if audio_classes == 0 {
        return Err("the module has no audio class".into());
    }
    Ok((manifest, audio_classes))
}
