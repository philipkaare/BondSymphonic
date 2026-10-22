use bondsymphonic_proto::{McpSecretValue, McpServerId, SecretRef};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Write, path::Path};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct SecretStore(pub BTreeMap<SecretRef, SecretRecord>);
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SecretRecord {
    pub owner: McpServerId,
    pub value: McpSecretValue,
}

pub(crate) fn is_private_temporary(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix(".secrets-")
        .and_then(|s| s.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut parts = body.rsplitn(3, '.');
    let counter = parts.next().unwrap_or("");
    let pid = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    !counter.is_empty()
        && counter.bytes().all(|c| c.is_ascii_digit())
        && !pid.is_empty()
        && pid.bytes().all(|c| c.is_ascii_digit())
        && !version.is_empty()
        && version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

/// The temporary itself is private from creation, including while bytes land.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = crate::util::atomic::temp_path(path);
    let result = (|| {
        #[cfg(not(windows))]
        let mut file = {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&tmp)?
        };
        #[cfg(windows)]
        let mut file = windows_private::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            if let Ok(directory) = std::fs::File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// Security attributes are passed to CreateFileW, so no credential bytes ever
/// exist under an inherited DACL. ACL/token conversion errors fail closed.
#[cfg(windows)]
mod windows_private {
    use std::{
        ffi::c_void,
        io,
        os::windows::{ffi::OsStrExt, io::FromRawHandle},
        path::Path,
    };
    type Handle = *mut c_void;
    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }
    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        attributes: u32,
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: u32,
            output: *mut c_void,
            length: u32,
            needed: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *const c_void, output: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            input: *const u16,
            revision: u32,
            output: *mut *mut c_void,
            length: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
        fn CreateFileW(
            path: *const u16,
            access: u32,
            share: u32,
            security: *const SecurityAttributes,
            creation: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
    }
    struct OwnedHandle(Handle);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    struct LocalAllocation(*mut c_void);
    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    unsafe fn wide_string(pointer: *const u16) -> String {
        let mut len = 0;
        while unsafe { *pointer.add(len) } != 0 {
            len += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, len) })
    }
    pub(super) fn create(path: &Path) -> io::Result<std::fs::File> {
        // All out pointers refer to initialized locals; TokenUser is allocated
        // pointer-aligned and kept alive through SID conversion. LocalFree owns
        // API allocations, CloseHandle owns the token, and the successful file
        // handle is transferred exactly once into File. Descriptor lifetime
        // covers CreateFileW, which copies the security attributes at creation.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), 0x0008, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = OwnedHandle(token);
            let mut needed = 0;
            GetTokenInformation(token.0, 1, std::ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }
            // TokenUser starts with a pointer, so allocate pointer-aligned memory.
            let mut user = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
            if GetTokenInformation(
                token.0,
                1,
                user.as_mut_ptr().cast(),
                (user.len() * std::mem::size_of::<usize>()) as u32,
                &mut needed,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let sid = (*(user.as_ptr().cast::<SidAndAttributes>())).sid;
            let mut sid_text = std::ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut sid_text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let sid_allocation = LocalAllocation(sid_text.cast());
            let sddl = format!(
                "D:P(A;;FA;;;{})(A;;FA;;;SY)(A;;FA;;;BA)",
                wide_string(sid_text)
            );
            drop(sid_allocation);
            let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
            let mut descriptor = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let descriptor = LocalAllocation(descriptor);
            let attributes = SecurityAttributes {
                length: std::mem::size_of::<SecurityAttributes>() as u32,
                descriptor: descriptor.0,
                inherit: 0,
            };
            let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // CREATE_NEW prevents replacing another writer; share=0 prevents
            // opening the temporary while the credential bytes are landing.
            let handle = CreateFileW(
                path.as_ptr(),
                0x40000000,
                0,
                &attributes,
                1,
                0x80,
                std::ptr::null_mut(),
            );
            if handle == (-1isize as Handle) {
                return Err(io::Error::last_os_error());
            }
            Ok(std::fs::File::from_raw_handle(handle))
        }
    }
}
