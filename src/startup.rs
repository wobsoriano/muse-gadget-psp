//! What the PSP reads to load the app, and the first code it runs.
//!
//! This is rust-psp's `module!` macro written out, with one line added. The
//! PSP hands the first thread the path the app was started from, and the
//! macro uses it only to set that thread's working folder. The app's other
//! threads have no working folder, so the path is kept for them here.

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};
use psp::sys::{self, SceLibAttr, SceLibraryEntry, SceLibraryEntryTable, SceModuleInfo, ThreadAttributes};

/// The path the app was started from, or null before `psp_main` runs.
pub static STARTED_FROM: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

const VERSION: (u8, u8) = (0, 2);

extern "C" {
    static _gp: u8;
    static __lib_ent_bottom: u8;
    static __lib_ent_top: u8;
    static __lib_stub_bottom: u8;
    static __lib_stub_top: u8;
}

#[no_mangle]
#[link_section = ".rodata.sceModuleInfo"]
#[used]
static MODULE_INFO: psp::Align16<SceModuleInfo> = psp::Align16(SceModuleInfo {
    mod_attribute: 0,
    mod_version: [VERSION.0, VERSION.1],
    mod_name: SceModuleInfo::name("muse"),
    terminal: 0,
    gp_value: unsafe { &_gp },
    stub_top: unsafe { &__lib_stub_top },
    stub_end: unsafe { &__lib_stub_bottom },
    ent_top: unsafe { &__lib_ent_top },
    ent_end: unsafe { &__lib_ent_bottom },
});

#[no_mangle]
#[link_section = ".lib.ent"]
#[used]
static LIB_ENT: SceLibraryEntry = SceLibraryEntry {
    name: core::ptr::null(),
    version: VERSION,
    attribute: SceLibAttr::SCE_LIB_IS_SYSLIB,
    entry_len: 4,
    var_count: 1,
    func_count: 1,
    entry_table: &LIB_ENT_TABLE,
};

#[no_mangle]
#[link_section = ".rodata.sceResident"]
#[used]
static LIB_ENT_TABLE: SceLibraryEntryTable = SceLibraryEntryTable {
    module_start_nid: 0xd632acdb,
    module_info_nid: 0xf01d73a7,
    module_start,
    module_info: &MODULE_INFO.0,
};

extern "C" fn main_thread(argc: usize, argv: *mut c_void) -> i32 {
    if argc > 0 {
        STARTED_FROM.store(argv as *mut u8, Ordering::Relaxed);
    }
    psp::_start!(crate::psp_main, argc, argv)
}

#[no_mangle]
extern "C" fn module_start(argc_bytes: usize, argv: *mut c_void) -> isize {
    unsafe {
        let id = sys::sceKernelCreateThread(
            b"main_thread\0".as_ptr(),
            main_thread,
            32,
            256 * 1024,
            ThreadAttributes::USER | ThreadAttributes::VFPU,
            core::ptr::null_mut(),
        );
        sys::sceKernelStartThread(id, argc_bytes, argv);
    }
    0
}
