use std::ptr;

use windows_sys::Win32::System::Threading::{
    AVRT_PRIORITY_CRITICAL, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, GetCurrentProcess,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
};

pub fn disable_power_throttling() {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            ptr::from_ref(&state).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

pub fn enter_pro_audio_thread() -> bool {
    let task_name: Vec<u16> = "Pro Audio".encode_utf16().chain(Some(0)).collect();
    let mut task_index = 0;
    let task = unsafe { AvSetMmThreadCharacteristicsW(task_name.as_ptr(), &mut task_index) };
    if task.is_null() {
        return false;
    }
    unsafe { AvSetMmThreadPriority(task, AVRT_PRIORITY_CRITICAL) };
    true
}
