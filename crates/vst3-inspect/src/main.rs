use std::ffi::c_char;
use std::path::Path;

use vault_plugin::Module;
use vst3::Steinberg::*;

fn text(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex(id: &TUID) -> String {
    id.iter().map(|b| format!("{:02X}", *b as u8)).collect()
}

fn inspect(path: &Path) -> Result<(), String> {
    let module = Module::load(path)?;
    let factory = module.factory();

    unsafe {
        let mut info: PFactoryInfo = std::mem::zeroed();
        if factory.getFactoryInfo(&mut info) == kResultOk {
            println!("factory: vendor={:?} url={:?}", text(&info.vendor), text(&info.url));
        }
        let factory2 = factory.cast::<IPluginFactory2>();
        println!("interfaces: factory2={} factory3={}", factory2.is_some(), factory.cast::<IPluginFactory3>().is_some());
        for index in 0..factory.countClasses() {
            let mut class: PClassInfo = std::mem::zeroed();
            if factory.getClassInfo(index, &mut class) != kResultOk {
                println!("class {index}: getClassInfo failed");
                continue;
            }
            println!("class {index}: cid={} category={:?} name={:?}", hex(&class.cid), text(&class.category), text(&class.name));
            if let Some(factory2) = &factory2 {
                let mut extra: PClassInfo2 = std::mem::zeroed();
                if factory2.getClassInfo2(index, &mut extra) == kResultOk {
                    println!(
                        "  sub={:?} vendor={:?} version={:?} sdk={:?} flags={:#x}",
                        text(&extra.subCategories),
                        text(&extra.vendor),
                        text(&extra.version),
                        text(&extra.sdkVersion),
                        extra.classFlags,
                    );
                }
            }
        }
    }
    Ok(())
}

fn main() -> ! {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: vst3-inspect PATH_TO_VST3");
        vault_plugin::end_process(2)
    };
    match inspect(Path::new(&path)) {
        Ok(()) => vault_plugin::end_process(0),
        Err(message) => {
            eprintln!("vst3-inspect: {message}");
            vault_plugin::end_process(1)
        }
    }
}
