use std::ffi::OsStr;
use std::time::{Duration, SystemTime};

use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, KernelConfig, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
    WriteFlags,
};
use tracing::{debug, error};

use crate::block_device::BlockDevice;

const ROOT_INODE: INodeNo = INodeNo::ROOT;
const DEVICE_INODE: INodeNo = INodeNo(2);
const TTL: Duration = Duration::from_secs(1);

pub struct IscsiFuseFs {
    block_device: BlockDevice,
    device_filename: String,
    read_only: bool,
    uid: u32,
    gid: u32,
}

impl IscsiFuseFs {
    pub fn new(
        block_device: BlockDevice,
        device_filename: String,
        read_only: bool,
        uid: u32,
        gid: u32,
    ) -> Self {
        Self {
            block_device,
            device_filename,
            read_only,
            uid,
            gid,
        }
    }

    pub fn fuse_config(read_only: bool, volume_name: &str) -> Config {
        let mut options = vec![
            MountOption::FSName("iscsi-fuse".to_string()),
            MountOption::Subtype("iscsi".to_string()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            // macFUSE: set volume name for Finder sidebar
            MountOption::CUSTOM(format!("volname={volume_name}")),
            // macFUSE: report as local volume so Finder shows disk icon
            MountOption::CUSTOM("local".to_string()),
        ];
        if read_only {
            options.push(MountOption::RO);
        } else {
            options.push(MountOption::RW);
        }

        let mut config = Config::default();
        config.mount_options = options;
        // macFUSE only supports single-threaded mode; Linux FUSE3 supports multi-threaded.
        #[cfg(target_os = "linux")]
        {
            config.n_threads = Some(num_cpus::get());
        }
        #[cfg(not(target_os = "linux"))]
        {
            config.n_threads = Some(1);
        }
        config
    }

    fn root_attr(&self) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino: ROOT_INODE,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    fn device_attr(&self) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino: DEVICE_INODE,
            size: self.block_device.total_bytes(),
            blocks: self.block_device.total_bytes() / 512,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::RegularFile,
            perm: if self.read_only { 0o444 } else { 0o644 },
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: self.block_device.block_size(),
            flags: 0,
        }
    }
}

impl Filesystem for IscsiFuseFs {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
        debug!("FUSE filesystem initialized");
        Ok(())
    }

    fn destroy(&mut self) {
        debug!("FUSE filesystem destroyed");
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent == ROOT_INODE && name.to_str() == Some(&self.device_filename) {
            reply.entry(&TTL, &self.device_attr(), Generation(0));
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match ino {
            i if i == ROOT_INODE => reply.attr(&TTL, &self.root_attr()),
            i if i == DEVICE_INODE => reply.attr(&TTL, &self.device_attr()),
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        match ino {
            i if i == ROOT_INODE => reply.attr(&TTL, &self.root_attr()),
            i if i == DEVICE_INODE => reply.attr(&TTL, &self.device_attr()),
            _ => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if ino != DEVICE_INODE {
            reply.error(Errno::ENOENT);
            return;
        }

        // FOPEN_DIRECT_IO bypasses the kernel page cache. This prevents
        // DiskImages/FUSE cache incoherence when newfs_apfs writes through
        // /dev/disk (buffered) and reads back through /dev/rdisk (raw).
        // Our userspace BlockCache replaces the kernel page cache.
        reply.opened(FileHandle(0), FopenFlags::FOPEN_DIRECT_IO);
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        if ino != DEVICE_INODE {
            reply.error(Errno::ENOENT);
            return;
        }

        match self.block_device.read_bytes(offset, size) {
            Ok(data) => reply.data(&data),
            Err(errno) => {
                error!(offset, size, "FUSE read failed");
                reply.error(errno);
            }
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        if ino != DEVICE_INODE {
            reply.error(Errno::ENOENT);
            return;
        }

        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }

        match self.block_device.write_bytes(offset, data) {
            Ok(written) => reply.written(written),
            Err(errno) => {
                error!(offset, size = data.len(), "FUSE write failed");
                reply.error(errno);
            }
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if ino != ROOT_INODE {
            reply.error(Errno::ENOENT);
            return;
        }

        let entries: Vec<(INodeNo, FileType, &str)> = vec![
            (ROOT_INODE, FileType::Directory, "."),
            (ROOT_INODE, FileType::Directory, ".."),
            (DEVICE_INODE, FileType::RegularFile, &self.device_filename),
        ];

        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let bs = self.block_device.block_size() as u64;
        let total = self.block_device.total_bytes();
        let blocks = total / bs;

        // When read-write, report all space as free so Finder shows usable capacity
        let (free, avail) = if self.read_only {
            (0, 0)
        } else {
            (blocks, blocks)
        };

        reply.statfs(
            blocks,    // total blocks
            free,      // free blocks
            avail,     // available blocks
            1,         // total inodes
            0,         // free inodes
            bs as u32, // block size
            255,       // max name length
            bs as u32, // fragment size
        );
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        if ino == DEVICE_INODE && !self.read_only {
            match self.block_device.flush() {
                Ok(()) => reply.ok(),
                Err(errno) => reply.error(errno),
            }
        } else {
            reply.ok();
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        if ino == DEVICE_INODE && !self.read_only {
            match self.block_device.flush() {
                Ok(()) => reply.ok(),
                Err(errno) => reply.error(errno),
            }
        } else {
            reply.ok();
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuse_config_multi_threaded() {
        let config = IscsiFuseFs::fuse_config(false, "test-vol");
        assert!(config.n_threads.unwrap() >= 1);
    }

    #[test]
    fn test_fuse_config_read_only() {
        let config = IscsiFuseFs::fuse_config(true, "test-vol");
        let has_ro = config
            .mount_options
            .iter()
            .any(|o| matches!(o, MountOption::RO));
        assert!(has_ro);
    }

    #[test]
    fn test_fuse_config_read_write() {
        let config = IscsiFuseFs::fuse_config(false, "test-vol");
        let has_rw = config
            .mount_options
            .iter()
            .any(|o| matches!(o, MountOption::RW));
        assert!(has_rw);
    }
}
