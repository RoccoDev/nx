use crate::result::*;
use crate::results;
use crate::mem;
use crate::service;
use crate::service::fspsrv;
use crate::service::fspsrv::IFileSystemProxy;
use crate::service::fspsrv::IFileSystem;
use crate::service::fspsrv::IFile;
use crate::service::fspsrv::IDirectory;
use crate::sync;
use crate::ipc::sf;
use alloc::vec::Vec;
use alloc::string::String;
use core::mem as cmem;

enum PathSegmentType {
    Invalid,
    Root,
    Normal
}

struct PathSegment {
    name: String,
    segment_type: PathSegmentType
}

impl PathSegment {
    pub const fn from(name: String, segment_type: PathSegmentType) -> Self {
        Self { name, segment_type }
    }

    pub const fn new() -> Self {
        Self::from(String::new(), PathSegmentType::Invalid)
    }
}

type UnpackedPath = Vec<PathSegment>;

fn unpack_path_impl(path: String) -> UnpackedPath {
    let mut unpacked_path: UnpackedPath = UnpackedPath::new();

    for sub_path in path.split('/') {
        let mut cur_segment = PathSegment::new();
        if sub_path.ends_with(':') {
            cur_segment.segment_type = PathSegmentType::Root;
            cur_segment.name = String::from(sub_path);
            unpacked_path.push(cur_segment);
        }
        else if sub_path == ".." {
            unpacked_path.pop();
        }
        else {
            cur_segment.segment_type = PathSegmentType::Normal;
            cur_segment.name = String::from(sub_path);
            unpacked_path.push(cur_segment);
        }
    }

    unpacked_path
}

fn unpack_path(path: String) -> Result<UnpackedPath> {
    let unpacked_path = unpack_path_impl(path);
    result_return_if!(unpacked_path.is_empty(), 0xBAD);
    Ok(unpacked_path)
}

fn pack_path(unpacked_path: UnpackedPath, add_root: bool) -> String {
    let mut path = String::new();
    if !add_root {
        path.push('/');
    }
    
    for path_segment in unpacked_path {
        match path_segment.segment_type {
            PathSegmentType::Root => {
                if add_root {
                    path = format!("{}{}/", path, path_segment.name);
                }
            },
            PathSegmentType::Normal => path = format!("{}{}/", path, path_segment.name),
            _ => {}
        }
    }
    
    // Minimum path must be "/"
    if path.len() > 1 {
        path.pop();
    }

    path
}

// TODO: use traits to use non-IPC filesystems

pub use fspsrv::FileAttribute;
pub use fspsrv::DirectoryEntryType;
pub use fspsrv::DirectoryOpenMode;

struct Device {
    root_name: PathSegment,
    fs: mem::Shared<fspsrv::FileSystem>
}

impl Device {
    pub fn from(root_name: PathSegment, fs: mem::Shared<fspsrv::FileSystem>) -> Self {
        Self { root_name, fs }
    }
}

pub struct File {
    file: mem::Shared<fspsrv::File>,
    offset: usize
}

pub enum Whence {
    Start,
    Current,
    End
}

impl File {
    pub fn new(file: mem::Shared<fspsrv::File>) -> Self {
        Self { file, offset: 0 }
    }

    pub fn get_size(&mut self) -> Result<usize> {
        self.file.get().get_size()
    }

    pub fn seek(&mut self, offset: usize, whence: Whence) -> Result<()> {
        match whence {
            Whence::Start => self.offset = offset,
            Whence::Current => self.offset += offset,
            Whence::End => {
                let size = self.get_size()?;
                self.offset = size + offset;
            }
        };
        Ok(())
    }

    pub fn read<T>(&mut self, buf: *mut T, size: usize) -> Result<usize> {
        let read_size = self.file.get().read(fspsrv::FileReadOption::None(), self.offset, size, sf::Buffer::from_mut(buf, size))?;
        self.offset += size;
        Ok(read_size)
    }

    pub fn read_array<T>(&mut self, arr: &mut [T]) -> Result<usize> {
        self.read(arr.as_mut_ptr(), arr.len() * cmem::size_of::<T>())
    }

    pub fn read_val<T: Copy + Default>(&mut self) -> Result<T> {
        let mut t: T = Default::default();
        self.read(&mut t, cmem::size_of::<T>())?;
        Ok(t)
    }

    pub fn write<T>(&mut self, buf: *const T, size: usize) -> Result<usize> {
        self.file.get().write(fspsrv::FileWriteOption::Flush(), self.offset, size, sf::Buffer::from_const(buf, size))?;
        self.offset += size;
        // Write command does not return the written size
        Ok(size)
    }

    pub fn write_array<T>(&mut self, arr: &[T]) -> Result<usize> {
        self.write(arr.as_ptr(), arr.len() * cmem::size_of::<T>())
    }

    pub fn write_val<T: Copy>(&mut self, t: T) -> Result<usize> {
        self.write(&t, cmem::size_of::<T>())
    }
}

pub struct Directory {
    dir: mem::Shared<fspsrv::Directory>,
    offset: usize,
    entry_count: usize,
    entries: Vec<fspsrv::DirectoryEntry>
}

impl Directory {
    pub fn new(dir: mem::Shared<fspsrv::Directory>) -> Result<Self> {
        let entry_count = dir.get().get_entry_count()?;

        Ok(Self { dir, offset: 0, entry_count: entry_count as usize, entries: Vec::new() })
    }

    fn refresh(&mut self) -> Result<()> {
        if self.offset >= self.entries.len() {
            let new_count = 16;
            let mut new_entries: Vec<fspsrv::DirectoryEntry> = vec![unsafe { core::mem::zeroed() }; new_count];
            let read = self.dir.get().read(sf::Buffer::from_array(&new_entries))?;
            new_entries.shrink_to(read as usize);

            self.entries.append(&mut new_entries);
        }

        Ok(())
    }

    pub fn rewind(&mut self) -> Result<()> {
        self.offset = 0;
        self.refresh()
    }

    pub fn rel(&self) -> (usize, usize) {
        (self.offset, self.entry_count)
    }

    pub fn next(&mut self) -> Result<Option<fspsrv::DirectoryEntry>> {
        if self.entries.len() == self.entry_count {
            Ok(None)
        }
        else {
            self.refresh()?;
            if self.offset == self.entry_count {
                Ok(None)
            }
            else {
                let entry = self.entries[self.offset];
                self.offset += 1;
                Ok(Some(entry))
            }
        }
    }
}

static mut G_FSPSRV_SESSION: sync::Locked<mem::Shared<fspsrv::FileSystemProxy>> = sync::Locked::new(false, mem::Shared::empty());
static mut G_DEVICES: sync::Locked<Vec<Device>> = sync::Locked::new(false, Vec::new());

fn find_device_by_name(name: &PathSegment) -> Result<mem::Shared<fspsrv::FileSystem>> {
    unsafe {
        for device in G_DEVICES.get() {
            if device.root_name.name == name.name {
                return Ok(device.fs.clone());
            }
        }
        Err(results::lib::fs::ResultDeviceNotFound::make())
    }
}

pub fn initialize() -> Result<()> {
    unsafe {
        G_FSPSRV_SESSION.set(service::new_service_object()?);
    }
    Ok(())
}

pub fn is_initialized() -> bool {
    unsafe {
        !G_FSPSRV_SESSION.get().is_null()
    }
}

pub fn finalize() {
    unsafe {
        G_DEVICES.get().clear();
        G_FSPSRV_SESSION.get().reset();
    }
}

pub fn mount(name: &str, fs: mem::Shared<fspsrv::FileSystem>) -> Result<()> {
    result_return_unless!(is_initialized(), results::lib::ResultNotInitialized);

    let root_name = PathSegment::from(format!("{}:", name), PathSegmentType::Root);
    unsafe {
        G_DEVICES.get().push(Device::from(root_name, fs));
    }

    Ok(())
}

pub fn mount_sd_card(name: &str) -> Result<()> {
    result_return_unless!(is_initialized(), results::lib::ResultNotInitialized);
    
    let sd_fs = unsafe { G_FSPSRV_SESSION.get().get().open_sd_card_filesystem()?.to::<fspsrv::FileSystem>() };
    mount(name, sd_fs)
}

pub fn unmount(name: &str) {
    let root_name = String::from(name);
    unsafe {
        G_DEVICES.get().retain(|dev| dev.root_name.name != root_name);
    }
}

pub fn create_file(path: String, size: usize, attribute: FileAttribute) -> Result<()> {
    result_return_unless!(is_initialized(), results::lib::ResultNotInitialized);

    let unpacked_path = unpack_path(path)?;
    let fs = find_device_by_name(unpacked_path.first().unwrap())?;
    let processed_path = pack_path(unpacked_path, false);
    let path_buf = fspsrv::Path::from_string(processed_path)?;
    fs.get().create_file(attribute, size, sf::Buffer::from_var(&path_buf))
}

pub fn delete_file(path: String) -> Result<()> {
    result_return_unless!(is_initialized(), results::lib::ResultNotInitialized);

    let unpacked_path = unpack_path(path)?;
    let fs = find_device_by_name(unpacked_path.first().unwrap())?;
    let processed_path = pack_path(unpacked_path, false);
    let path_buf = fspsrv::Path::from_string(processed_path)?;
    fs.get().delete_file(sf::Buffer::from_var(&path_buf))
}

pub fn create_directory(path: String) -> Result<()> {
    result_return_unless!(is_initialized(), results::lib::ResultNotInitialized);

    let unpacked_path = unpack_path(path)?;
    let fs = find_device_by_name(unpacked_path.first().unwrap())?;
    let processed_path = pack_path(unpacked_path, false);
    let path_buf = fspsrv::Path::from_string(processed_path)?;
    fs.get().create_directory(sf::Buffer::from_var(&path_buf))
}

pub fn delete_directory(path: String) -> Result<()> {
    result_return_unless!(is_initialized(), results::lib::ResultNotInitialized);

    let unpacked_path = unpack_path(path)?;
    let fs = find_device_by_name(unpacked_path.first().unwrap())?;
    let processed_path = pack_path(unpacked_path, false);
    let path_buf = fspsrv::Path::from_string(processed_path)?;
    fs.get().delete_directory_recursively(sf::Buffer::from_var(&path_buf))
}

pub fn get_entry_type(path: String) -> Result<DirectoryEntryType> {
    result_return_unless!(is_initialized(), results::lib::ResultNotInitialized);

    let unpacked_path = unpack_path(path)?;
    let fs = find_device_by_name(unpacked_path.first().unwrap())?;
    let processed_path = pack_path(unpacked_path, false);
    let path_buf = fspsrv::Path::from_string(processed_path)?;
    fs.get().get_entry_type(sf::Buffer::from_var(&path_buf))
}

bit_enum! {
    FileOpenOption (u32) {
        Create = bit!(0),
        Read = bit!(1),
        Write = bit!(2),
        Append = bit!(3)
    }
}

fn convert_file_open_option(option: FileOpenOption) -> fspsrv::FileOpenMode {
    let mut mode = fspsrv::FileOpenMode::None();
    if option.contains(FileOpenOption::Read()) {
        mode |= fspsrv::FileOpenMode::Read();
    }
    if option.contains(FileOpenOption::Write()) {
        mode |= fspsrv::FileOpenMode::Write();
    }
    if option.contains(FileOpenOption::Append()) {
        mode |= fspsrv::FileOpenMode::Append();
    }
    mode
}

pub fn open_file(path: String, option: FileOpenOption) -> Result<File> {
    result_return_unless!(is_initialized(), results::lib::ResultNotInitialized);

    let unpacked_path = unpack_path(path)?;
    let fs = find_device_by_name(unpacked_path.first().unwrap())?;
    let processed_path = pack_path(unpacked_path, false);
    let path_buf = fspsrv::Path::from_string(processed_path)?;

    let mode = convert_file_open_option(option);
    let file = match fs.get().open_file(mode, sf::Buffer::from_var(&path_buf)) {
        Ok(file_obj) => file_obj.to::<fspsrv::File>(),
        Err(rc) => {
            if results::fs::ResultPathNotFound::matches(rc) && option.contains(FileOpenOption::Create()) {
                // Create the file if it doesn't exist and we were told to do so
                fs.get().create_file(FileAttribute::None(), 0, sf::Buffer::from_var(&path_buf))?;
                fs.get().open_file(mode, sf::Buffer::from_var(&path_buf))?.to::<fspsrv::File>()
            }
            else {
                return Err(rc);
            }
        }
    };
    let offset : usize = match option.contains(FileOpenOption::Append()) {
        true => file.get().get_size().unwrap_or(0),
        false => 0
    };

    Ok(File { file, offset })
}

pub fn open_directory(path: String, mode: fspsrv::DirectoryOpenMode) -> Result<Directory> {
    result_return_unless!(is_initialized(), results::lib::ResultNotInitialized);

    let unpacked_path = unpack_path(path)?;
    let fs = find_device_by_name(unpacked_path.first().unwrap())?;
    let processed_path = pack_path(unpacked_path, false);
    let path_buf = fspsrv::Path::from_string(processed_path)?;

    let dir = fs.get().open_directory(mode, sf::Buffer::from_var(&path_buf))?.to::<fspsrv::Directory>();
    Directory::new(dir)
}

pub fn format_path(path: String) -> Result<(mem::Shared<fspsrv::FileSystem>, String)> {
    result_return_unless!(is_initialized(), results::lib::ResultNotInitialized);

    let unpacked_path = unpack_path(path)?;
    let fs = find_device_by_name(unpacked_path.first().unwrap())?;
    let processed_path = pack_path(unpacked_path, false);
    
    Ok((fs, processed_path))
}