//! WinFSP read-timeout spike (Phase 2 commit 3; design
//! `docs/dev/winfsp-backend.md` §4.4).
//!
//! Mounts a minimal filesystem — one 1 KiB file whose `read` callback
//! sleeps for a configured duration, standing in for on-demand tile
//! generation — and issues a synchronous `ReadFile` against it. Records
//! whether the read completes or the driver's pending-IRP ceiling
//! (`VolumeParams::IrpTimeout`) aborts it, the resulting error, and
//! whether the volume still answers a cheap metadata query afterwards.
//!
//! Usage: `winfsp_spike <sleep_ms> <irp_timeout_ms>`
//! (defaults: 5000 / 60000)
//!
//! Windows in effect: it needs the WinFSP runtime and mounts a temp
//! folder. On other platforms it compiles to a no-op `main` — CI builds
//! the example on every OS but never runs it.

fn main() {
    #[cfg(windows)]
    spike::run();

    #[cfg(not(windows))]
    {
        println!("winfsp_spike is Windows-only (it needs the WinFSP runtime); nothing to do here.");
    }
}

/// The Windows spike, compiled only where the WinFSP bindings exist.
#[cfg(windows)]
mod spike {
    use std::ffi::c_void;
    use std::io::ErrorKind;
    use std::process;
    use std::time::{Duration, Instant};

    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY};
    use winfsp::filesystem::{FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo};
    use winfsp::host::{FileSystemHost, VolumeParams};
    use winfsp::{winfsp_init_or_die, FspError, Result, U16CStr};

    const FILE_NAME: &str = "spike.bin";
    const FILE_SIZE: u64 = 1024;

    /// Which of the volume's two entries a handle refers to.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Dir,
        File,
    }

    /// The whole volume: one directory containing [`FILE_NAME`].
    #[derive(Debug)]
    struct SpikeFS {
        /// How long `read` blocks before answering — the generation stand-in.
        sleep: Duration,
    }

    /// Resolve a driver path (`""`, `"\spike.bin"`, `"spike.bin"`, ...) to a
    /// volume entry. Everything else is unknown.
    fn known_name(file_name: &U16CStr) -> Option<Kind> {
        // A non-UTF-8 name can never match a volume entry.
        let Ok(s) = file_name.to_string() else {
            return None;
        };
        let s = s
            .trim_matches(|c| c == '\\' || c == '/')
            .to_ascii_lowercase();
        if s.is_empty() {
            Some(Kind::Dir)
        } else if s == FILE_NAME {
            Some(Kind::File)
        } else {
            None
        }
    }

    /// Read-only security for the two known entries; the driver applies its
    /// default ACL (`sz_security_descriptor == 0`), the same policy the real
    /// backend will use (design §4.3).
    fn security_for(kind: Kind) -> FileSecurity {
        FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes: match kind {
                Kind::File => FILE_ATTRIBUTE_READONLY.0,
                Kind::Dir => FILE_ATTRIBUTE_DIRECTORY.0,
            },
        }
    }

    impl FileSystemContext for SpikeFS {
        type FileContext = Kind;

        fn get_security_by_name(
            &self,
            file_name: &U16CStr,
            _security_descriptor: Option<&mut [c_void]>,
            _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
        ) -> Result<FileSecurity> {
            match known_name(file_name) {
                Some(kind) => Ok(security_for(kind)),
                None => Err(FspError::IO(ErrorKind::NotFound)),
            }
        }

        fn open(
            &self,
            file_name: &U16CStr,
            _create_options: u32,
            _granted_access: u32,
            file_info: &mut OpenFileInfo,
        ) -> Result<Self::FileContext> {
            match known_name(file_name) {
                Some(Kind::Dir) => Ok(Kind::Dir),
                Some(Kind::File) => {
                    let info = file_info.as_mut();
                    info.file_attributes = FILE_ATTRIBUTE_READONLY.0;
                    info.file_size = FILE_SIZE;
                    info.allocation_size = FILE_SIZE;
                    Ok(Kind::File)
                }
                None => Err(FspError::IO(ErrorKind::NotFound)),
            }
        }

        fn close(&self, _context: Self::FileContext) {}

        fn get_file_info(
            &self,
            context: &Self::FileContext,
            file_info: &mut FileInfo,
        ) -> Result<()> {
            match context {
                Kind::File => {
                    file_info.file_attributes = FILE_ATTRIBUTE_READONLY.0;
                    file_info.file_size = FILE_SIZE;
                    file_info.allocation_size = FILE_SIZE;
                    Ok(())
                }
                Kind::Dir => Ok(()),
            }
        }

        fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
            if *context != Kind::File {
                return Err(FspError::IO(ErrorKind::NotADirectory));
            }
            let start = (offset as u64).min(FILE_SIZE) as usize;
            let end = (offset.saturating_add(buffer.len() as u64)).min(FILE_SIZE) as usize;
            if start >= end {
                // An EOF probe: answer immediately (a real backend would not do
                // generation work here), and log it so the IRP count is visible.
                println!("  read-irp: offset={offset} len={} -> EOF(0)", buffer.len());
                return Ok(0);
            }
            // The spike's whole point: block past (or within) the IRP ceiling,
            // exactly like a stalled tile generation would.
            println!(
                "  read-irp: offset={offset} len={} -> blocking {:?}",
                buffer.len(),
                self.sleep
            );
            std::thread::sleep(self.sleep);
            buffer[..end - start].fill(0xAB);
            println!("  read-irp: -> OK {} bytes", end - start);
            Ok((end - start) as u32)
        }

        fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> Result<()> {
            out_volume_info.total_size = FILE_SIZE;
            out_volume_info.free_size = 0;
            Ok(())
        }
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        let sleep_ms: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5_000);
        let irp_ms: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60_000);
        println!("winfsp_spike: sleep={sleep_ms}ms irp_timeout={irp_ms}ms");

        winfsp_init_or_die();

        // A fresh mount point per run (a unique name also dodges any stale mount
        // left by a killed run). Do NOT pre-create the directory: WinFSP's
        // directory mount creates it itself (FILE_CREATE + DELETE_ON_CLOSE) and
        // removes it on unmount; a pre-existing name makes
        // FspFileSystemSetMountPoint fail with STATUS_OBJECT_NAME_COLLISION
        // (0xC0000035, HRESULT 0xD0000035).
        let mount_dir = std::env::temp_dir().join(format!("xel_winfsp_spike_{}", process::id()));

        let mut params = VolumeParams::new();
        params
            .filesystem_name("XEL spike")
            .sector_size(512)
            .irp_timeout(irp_ms as u32);

        let fs = SpikeFS {
            sleep: Duration::from_millis(sleep_ms),
        };

        // `new` is generic over the guard strategy and `start` is implemented per
        // strategy, so annotate the type (default: FineGuard — callbacks may run
        // concurrently on dispatcher threads; SpikeFS and Kind are Sync).
        let mut host: FileSystemHost<SpikeFS> = match FileSystemHost::new(params, fs) {
            Ok(host) => host,
            Err(e) => {
                eprintln!("FspFileSystemCreate failed: {e:?}");
                process::exit(1);
            }
        };
        if let Err(e) = host.start() {
            eprintln!("dispatcher start failed: {e:?}");
            process::exit(1);
        }
        if let Err(e) = host.mount(mount_dir.as_os_str()) {
            eprintln!("mount failed: {e:?}");
            process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(200)); // let the mount settle

        let target = mount_dir.join(FILE_NAME);

        // 1) The read under test: one blocking callback of `sleep_ms`.
        let t0 = Instant::now();
        let read = std::fs::read(&target);
        let read_elapsed = t0.elapsed();

        // 2) Did the volume survive? A cheap query — metadata needs no `read`
        //    callback, so it cannot sleep.
        let t1 = Instant::now();
        let meta = std::fs::metadata(&target);
        let meta_elapsed = t1.elapsed();

        match &read {
            Ok(bytes) => println!(
                "READ=OK bytes={} elapsed={:?} | FOLLOWUP-META=OK elapsed={:?}",
                bytes.len(),
                read_elapsed,
                meta_elapsed
            ),
            Err(e) => println!(
                "READ=FAIL kind={:?} raw_os_error={:?} elapsed={:?} | FOLLOWUP-META={} elapsed={:?}",
                e.kind(),
                e.raw_os_error(),
                read_elapsed,
                if meta.is_ok() { "OK" } else { "FAIL" },
                meta_elapsed
            ),
        }

        host.unmount();
        host.stop();

        // The file must be unreachable once the mount is gone (WinFSP removed
        // the mount-point directory on unmount).
        let gone = std::fs::metadata(&target).is_err();
        println!("UNMOUNT={}", if gone { "OK" } else { "STILL-MOUNTED" });
    }
}
