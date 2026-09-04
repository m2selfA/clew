use std::{
    ffi::c_void,
    io,
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        Authorization::{
            ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE,
            SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
            TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
        },
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetSecurityDescriptorControl, INHERITED_ACE, OBJECT_INHERIT_ACE,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED,
    },
    Storage::FileSystem::FILE_ALL_ACCESS,
    System::SystemServices::ACCESS_ALLOWED_ACE_TYPE,
};

const SYSTEM_SID: &str = "S-1-5-18";
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
const EXPECTED_ACE_COUNT: u32 = 3;
const EXPECTED_INHERITANCE: u32 = OBJECT_INHERIT_ACE as u32 | CONTAINER_INHERIT_ACE as u32;

struct LocalAllocation(*mut c_void);

impl LocalAllocation {
    fn new(pointer: *mut c_void) -> Self {
        Self(pointer)
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

struct OwnedSid(LocalAllocation);

impl OwnedSid {
    fn parse(value: &str) -> io::Result<Self> {
        if value.is_empty() || value.encode_utf16().any(|unit| unit == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows SID is empty or contains NUL",
            ));
        }
        let mut wide = value.encode_utf16().collect::<Vec<_>>();
        wide.push(0);
        let mut sid: PSID = null_mut();
        let converted = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) };
        if converted == 0 || sid.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(LocalAllocation::new(sid.cast())))
    }

    fn as_psid(&self) -> PSID {
        self.0.as_ptr().cast()
    }
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.is_empty() || wide.iter().any(|unit| *unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows ACL path is empty or contains NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn trustee_for_sid(sid: PSID) -> TRUSTEE_W {
    TRUSTEE_W {
        pMultipleTrustee: null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid.cast(),
    }
}

fn explicit_access(sid: PSID) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: EXPECTED_INHERITANCE,
        Trustee: trustee_for_sid(sid),
    }
}

pub fn apply_protected_directory_dacl(path: &Path, service_sid: &str) -> io::Result<()> {
    let system = OwnedSid::parse(SYSTEM_SID)?;
    let administrators = OwnedSid::parse(ADMINISTRATORS_SID)?;
    let service = OwnedSid::parse(service_sid)?;
    let entries = [
        explicit_access(system.as_psid()),
        explicit_access(administrators.as_psid()),
        explicit_access(service.as_psid()),
    ];
    let mut acl: *mut ACL = null_mut();
    let result =
        unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), null(), &mut acl) };
    if result != ERROR_SUCCESS || acl.is_null() {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let acl_allocation = LocalAllocation::new(acl.cast());
    let mut path_wide = wide_path(path)?;
    let result = unsafe {
        SetNamedSecurityInfoW(
            path_wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl_allocation.as_ptr().cast(),
            null_mut(),
        )
    };
    if result != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    verify_protected_directory_dacl(path, service_sid)
}

pub fn verify_protected_directory_dacl(path: &Path, service_sid: &str) -> io::Result<()> {
    let expected = [
        OwnedSid::parse(SYSTEM_SID)?,
        OwnedSid::parse(ADMINISTRATORS_SID)?,
        OwnedSid::parse(service_sid)?,
    ];
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: *mut c_void = null_mut();
    let mut path_wide = wide_path(path)?;
    let result = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if result != ERROR_SUCCESS || descriptor.is_null() || dacl.is_null() {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let _descriptor = LocalAllocation::new(descriptor);

    let mut control = 0_u16;
    let mut revision = 0_u32;
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows machine service DACL is not protected from inheritance",
        ));
    }

    let mut size = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if size.AceCount != EXPECTED_ACE_COUNT {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Windows machine service DACL has {} ACEs; expected {EXPECTED_ACE_COUNT}",
                size.AceCount
            ),
        ));
    }

    let mut seen = [false; EXPECTED_ACE_COUNT as usize];
    for index in 0..size.AceCount {
        let mut raw_ace: *mut c_void = null_mut();
        if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 || raw_ace.is_null() {
            return Err(io::Error::last_os_error());
        }
        let ace = raw_ace.cast::<ACCESS_ALLOWED_ACE>();
        let header = unsafe { &(*ace).Header };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8
            || header.AceFlags as u32 & INHERITED_ACE as u32 != 0
            || header.AceFlags as u32 & EXPECTED_INHERITANCE != EXPECTED_INHERITANCE
            || unsafe { (*ace).Mask } != FILE_ALL_ACCESS
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows machine service DACL contains an unexpected ACE",
            ));
        }
        let sid = unsafe { std::ptr::addr_of!((*ace).SidStart) as PSID };
        let mut matched = None;
        for (expected_index, expected_sid) in expected.iter().enumerate() {
            if unsafe { EqualSid(sid, expected_sid.as_psid()) } != 0 {
                matched = Some(expected_index);
                break;
            }
        }
        let Some(expected_index) = matched else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows machine service DACL contains an unexpected SID",
            ));
        };
        if seen[expected_index] {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows machine service DACL contains a duplicate SID",
            ));
        }
        seen[expected_index] = true;
    }
    if !seen.into_iter().all(|value| value) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows machine service DACL is missing an expected SID",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_unregistered_service_sid_can_protect_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("machine-state");
        std::fs::create_dir(&root).unwrap();
        let synthetic_service_sid = "S-1-5-80-1-2-3-4-5";
        apply_protected_directory_dacl(&root, synthetic_service_sid).unwrap();
        verify_protected_directory_dacl(&root, synthetic_service_sid).unwrap();
    }
}
