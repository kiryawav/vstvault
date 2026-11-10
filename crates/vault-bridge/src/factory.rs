use std::ffi::c_void;

use vault_protocol::manifest::Manifest;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper};

use crate::instance::Instance;
use crate::text::{copy_utf8, copy_utf16};
use crate::{bridge_manifest, log_error};

pub struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory3,);
}

fn cid_as_tuid(cid: &[u8; 16]) -> TUID {
    cid.map(|byte| byte as char8)
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let Some(manifest) = bridge_manifest() else { return kResultFalse };
        let info = unsafe { &mut *info };
        copy_utf8(&manifest.factory_vendor, &mut info.vendor);
        copy_utf8(&manifest.factory_url, &mut info.url);
        copy_utf8(&manifest.factory_email, &mut info.email);
        info.flags = manifest.factory_flags;
        kResultOk
    }

    unsafe fn countClasses(&self) -> int32 {
        i32::from(bridge_manifest().is_some())
    }

    unsafe fn getClassInfo(&self, index: int32, info: *mut PClassInfo) -> tresult {
        let Some(manifest) = bridge_manifest().filter(|_| index == 0) else { return kInvalidArgument };
        let info = unsafe { &mut *info };
        info.cid = cid_as_tuid(&manifest.cid);
        info.cardinality = manifest.cardinality;
        copy_utf8(&manifest.category, &mut info.category);
        copy_utf8(&manifest.name, &mut info.name);
        kResultOk
    }

    unsafe fn createInstance(&self, cid: FIDString, iid: FIDString, object: *mut *mut c_void) -> tresult {
        unsafe { *object = std::ptr::null_mut() };
        let Some(manifest) = bridge_manifest() else { return kResultFalse };
        let requested = unsafe { *cid.cast::<TUID>() };
        if requested != cid_as_tuid(&manifest.cid) {
            return kInvalidArgument;
        }
        create_instance(manifest, iid, object)
    }
}

fn create_instance(manifest: &'static Manifest, iid: FIDString, object: *mut *mut c_void) -> tresult {
    let Some(instance) = ComWrapper::new(Instance::new(manifest)).to_com_ptr::<FUnknown>() else {
        log_error("cannot create the bridge instance");
        return kResultFalse;
    };
    let pointer = instance.as_ptr();
    unsafe { ((*(*pointer).vtbl).queryInterface)(pointer, iid.cast_mut().cast::<TUID>().cast_const(), object) }
}

impl IPluginFactory2Trait for Factory {
    unsafe fn getClassInfo2(&self, index: int32, info: *mut PClassInfo2) -> tresult {
        let Some(manifest) = bridge_manifest().filter(|_| index == 0) else { return kInvalidArgument };
        let info = unsafe { &mut *info };
        info.cid = cid_as_tuid(&manifest.cid);
        info.cardinality = manifest.cardinality;
        copy_utf8(&manifest.category, &mut info.category);
        copy_utf8(&manifest.name, &mut info.name);
        info.classFlags = manifest.class_flags;
        copy_utf8(&manifest.sub_categories, &mut info.subCategories);
        copy_utf8(&manifest.vendor, &mut info.vendor);
        copy_utf8(&manifest.version, &mut info.version);
        copy_utf8(&manifest.sdk_version, &mut info.sdkVersion);
        kResultOk
    }
}

impl IPluginFactory3Trait for Factory {
    unsafe fn getClassInfoUnicode(&self, index: int32, info: *mut PClassInfoW) -> tresult {
        let Some(manifest) = bridge_manifest().filter(|_| index == 0) else { return kInvalidArgument };
        let info = unsafe { &mut *info };
        info.cid = cid_as_tuid(&manifest.cid);
        info.cardinality = manifest.cardinality;
        copy_utf8(&manifest.category, &mut info.category);
        copy_utf16(&manifest.name, &mut info.name);
        info.classFlags = manifest.class_flags;
        copy_utf8(&manifest.sub_categories, &mut info.subCategories);
        copy_utf16(&manifest.vendor, &mut info.vendor);
        copy_utf16(&manifest.version, &mut info.version);
        copy_utf16(&manifest.sdk_version, &mut info.sdkVersion);
        kResultOk
    }

    unsafe fn setHostContext(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
}
