use crate::result::*;
use crate::results;
use crate::svc;
use crate::mem::alloc;
use crate::dynamic;
use crate::svc::Handle;
use crate::sync;
use crate::util;
use crate::hbl;
use crate::thread;
use crate::vmem;
use crate::version;
use crate::ipc::sf;
use crate::service;
use crate::service::set;
use crate::service::set::ISystemSettingsServer;
use core::ptr;

// These functions must be implemented by any executable homebrew project using this crate
extern "Rust" {
    fn main() -> Result<()>;
    fn initialize_heap(hbl_heap: util::PointerAndSize) -> util::PointerAndSize;
}

pub type ExitFn = extern "C" fn(ResultCode) -> !;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum ExecutableType {
    #[default]
    None,
    Nso,
    Nro
}

static mut G_EXECUTABLE_TYPE: ExecutableType = ExecutableType::None;

pub(crate) fn set_executable_type(exec_type: ExecutableType) {
    unsafe {
        G_EXECUTABLE_TYPE = exec_type;
    }
}

pub fn get_executable_type() -> ExecutableType {
    unsafe {
        G_EXECUTABLE_TYPE
    }
}

static mut G_EXIT_FN: sync::Locked<Option<ExitFn>> = sync::Locked::new(false, None);
static mut G_MAIN_THREAD: thread::Thread = thread::Thread::empty();

// TODO: consider adding a default heap-init function?

#[no_mangle]
#[linkage = "weak"]
fn initialize_version(hbl_hos_version: hbl::Version) {
    if hbl_hos_version.is_valid() {
        version::set_version(hbl_hos_version.to_version());
    }
    else {
        let set_sys = service::new_service_object::<set::SystemSettingsServer>().unwrap();
        let fw_version: set::FirmwareVersion = Default::default();
        set_sys.get().get_firmware_version(sf::Buffer::from_var(&fw_version)).unwrap();

        let version = version::Version::new(fw_version.major, fw_version.minor, fw_version.micro);
        version::set_version(version);
    }
}

#[no_mangle]
#[linkage = "weak"]
unsafe extern "C" fn __nx_rrt0_entry(abi_ptr: *const hbl::AbiConfigEntry, raw_main_thread_handle: u64, aslr_base_address: *const u8, lr_exit_fn: ExitFn) {
    // First of all, relocate ourselves
    dynamic::relocate(aslr_base_address).unwrap();
    
    let exec_type = match !abi_ptr.is_null() && (raw_main_thread_handle == u64::MAX) {
        true => ExecutableType::Nro,
        false => ExecutableType::Nso
    };
    set_executable_type(exec_type);

    let mut heap = util::PointerAndSize::new(ptr::null_mut(), 0);
    let mut main_thread_handle = raw_main_thread_handle as svc::Handle;
    let mut hos_version = hbl::Version::empty();

    // If we are a NRO, parse the config entries hbloader sent us
    if exec_type == ExecutableType::Nro {
        let mut abi_entry = abi_ptr;
        loop {
            match (*abi_entry).key {
                hbl::AbiConfigEntryKey::EndOfList => {
                    let loader_info_data = (*abi_entry).value[0] as *mut u8;
                    let loader_info_data_len = (*abi_entry).value[1] as usize;
                    if loader_info_data_len > 0 {
                        let loader_info_slice = core::slice::from_raw_parts(loader_info_data, loader_info_data_len);
                        if let Ok(loader_info) = core::str::from_utf8(loader_info_slice) {
                            hbl::set_loader_info(loader_info);
                        }
                    }
                    break;
                },
                hbl::AbiConfigEntryKey::MainThreadHandle => {
                    main_thread_handle = (*abi_entry).value[0] as svc::Handle;
                },
                hbl::AbiConfigEntryKey::NextLoadPath => {
                    let next_load_path_data = (*abi_entry).value[0] as *mut u8;
                    let next_load_path_data_len = util::str_ptr_len(next_load_path_data as *const u8);
                    let next_load_argv_data = (*abi_entry).value[1] as *mut u8;
                    let next_load_argv_data_len = util::str_ptr_len(next_load_argv_data as *const u8);
                    
                    let next_load_path_slice = core::slice::from_raw_parts(next_load_path_data, next_load_path_data_len);
                    let next_load_argv_slice = core::slice::from_raw_parts(next_load_argv_data, next_load_argv_data_len);
                    if let Ok(next_load_path) = core::str::from_utf8(next_load_path_slice) {
                        if let Ok(next_load_argv) = core::str::from_utf8(next_load_argv_slice) {
                            hbl::set_next_load_entry_ptr(next_load_path, next_load_argv);
                        }
                    }
                },
                hbl::AbiConfigEntryKey::OverrideHeap => {
                    heap.address = (*abi_entry).value[0] as *mut u8;
                    heap.size = (*abi_entry).value[1] as usize;
                },
                hbl::AbiConfigEntryKey::OverrideService => {
                    // todo!("OverrideService");
                },
                hbl::AbiConfigEntryKey::Argv => {
                    // todo!("Argv");
                },
                hbl::AbiConfigEntryKey::SyscallAvailableHint => {
                    // todo!("SyscallAvailableHint");
                },
                hbl::AbiConfigEntryKey::AppletType => {
                    let applet_type: hbl::AppletType = core::mem::transmute((*abi_entry).value[0] as u32);
                    hbl::set_applet_type(applet_type);
                },
                hbl::AbiConfigEntryKey::ProcessHandle => {
                    let proc_handle = (*abi_entry).value[0] as Handle;
                    hbl::set_process_handle(proc_handle);
                },
                hbl::AbiConfigEntryKey::LastLoadResult => {
                    let last_load_rc = ResultCode::new((*abi_entry).value[0] as u32);
                    hbl::set_last_load_result(last_load_rc);
                },
                hbl::AbiConfigEntryKey::RandomSeed => {
                    let random_seed = ((*abi_entry).value[0], (*abi_entry).value[1]);
                    hbl::set_random_seed(random_seed);
                },
                hbl::AbiConfigEntryKey::UserIdStorage => {
                    // todo!("UserIdStorage");
                },
                hbl::AbiConfigEntryKey::HosVersion => {
                    let hos_version_v = (*abi_entry).value[0] as u32;
                    let is_ams_magic = (*abi_entry).value[1];
                    hos_version = hbl::Version::new(hos_version_v, is_ams_magic);
                },
                _ => {
                    // TODO: invalid config entries?
                }
            }
            abi_entry = abi_entry.offset(1);
        }
    }

    // Initialize the main thread object and initialize its TLS section
    // TODO: query memory for main thread stack address/size?
    G_MAIN_THREAD = thread::Thread::new_remote(main_thread_handle, "MainThread", ptr::null_mut(), 0).unwrap();
    thread::set_current_thread(&mut G_MAIN_THREAD);

    // Initialize virtual memory
    vmem::initialize().unwrap();

    // Set exit function (will be null for non-hbl NROs)
    match exec_type {
        ExecutableType::Nro => G_EXIT_FN.set(Some(lr_exit_fn)),
        ExecutableType::Nso => G_EXIT_FN.set(None),
        _ => {}
    };
    
    // Initialize heap and memory allocation
    heap = initialize_heap(heap);
    alloc::initialize(heap);

    // Initialize version support
    initialize_version(hos_version);

    // TODO: finish implementing CRT0

    // Unwrap main(), which will trigger a panic if it didn't succeed
    main().unwrap();

    // Successful exit by default
    exit(ResultSuccess::make());
}

#[no_mangle]
#[linkage = "weak"]
unsafe extern "C" fn __nx_rrt0_exception_entry(_error_desc: u32, _stack_top: *mut u8) {
    svc::return_from_exception(results::os::ResultUnhandledException::make());
}

pub fn exit(rc: ResultCode) -> ! {
    unsafe {
        match G_EXIT_FN.get() {
            Some(exit_fn) => exit_fn(rc),
            None => svc::exit_process()
        }
    }
}
