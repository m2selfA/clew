use std::{
    ffi::c_void,
    io,
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE,
            SET_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorDacl,
        GetTokenInformation, InitializeSecurityDescriptor, PSID, SECURITY_ATTRIBUTES,
        SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{FILE_ALL_ACCESS, FILE_GENERIC_READ, FILE_GENERIC_WRITE, READ_CONTROL},
    System::{
        Services::{
            QueryServiceObjectSecurity, SERVICE_ALL_ACCESS, SERVICE_INTERROGATE,
            SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STOP,
            SetServiceObjectSecurity,
        },
        SystemServices::{ACCESS_ALLOWED_ACE_TYPE, SECURITY_DESCRIPTOR_REVISION},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

const SYSTEM_SID: &str = "S-1-5-18";
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
const SERVICE_SID_PREFIX: &str = "S-1-5-80-";
const MAX_SID_STRING_UNITS: usize = 256;

pub const AUTHORIZED_SERVICE_ACCESS: u32 = SERVICE_QUERY_CONFIG
    | SERVICE_QUERY_STATUS
    | SERVICE_START
    | SERVICE_STOP
    | SERVICE_INTERROGATE
    | READ_CONTROL;

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

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
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

fn sid_to_string(sid: PSID) -> io::Result<String> {
    if sid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows SID pointer is null",
        ));
    }
    let mut encoded: *mut u16 = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut encoded) } == 0 || encoded.is_null() {
        return Err(io::Error::last_os_error());
    }
    let allocation = LocalAllocation::new(encoded.cast());
    let mut length = 0_usize;
    while length < MAX_SID_STRING_UNITS {
        if unsafe { *encoded.add(length) } == 0 {
            let units = unsafe { std::slice::from_raw_parts(encoded, length) };
            return String::from_utf16(units).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows SID is not valid UTF-16",
                )
            });
        }
        length += 1;
    }
    drop(allocation);
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "Windows SID string exceeds the bounded length",
    ))
}

pub fn current_user_sid_string() -> io::Result<String> {
    let mut token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0
        || token.is_null()
    {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);

    let mut required = 0_u32;
    unsafe {
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required);
    }
    if required < std::mem::size_of::<TOKEN_USER>() as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows process token did not report a valid TokenUser size",
        ));
    }
    let mut buffer = vec![0_u8; required as usize];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let token_user = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let sid = sid_to_string(token_user.User.Sid)?;
    validate_control_user_sid(&sid)?;
    Ok(sid)
}

pub fn validate_control_user_sid(value: &str) -> io::Result<()> {
    let _ = OwnedSid::parse(value)?;
    if value.eq_ignore_ascii_case(SYSTEM_SID)
        || value.eq_ignore_ascii_case(ADMINISTRATORS_SID)
        || value
            .to_ascii_uppercase()
            .starts_with(&SERVICE_SID_PREFIX.to_ascii_uppercase())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows machine control SID must identify a user account, not SYSTEM, Administrators, or a service SID",
        ));
    }
    Ok(())
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

fn explicit_access(sid: PSID, mask: u32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: SET_ACCESS,
        grfInheritance: 0,
        Trustee: trustee_for_sid(sid),
    }
}

fn build_acl(entries: &[(PSID, u32)]) -> io::Result<LocalAllocation> {
    let explicit = entries
        .iter()
        .map(|(sid, mask)| explicit_access(*sid, *mask))
        .collect::<Vec<_>>();
    let mut acl: *mut ACL = null_mut();
    let result =
        unsafe { SetEntriesInAclW(explicit.len() as u32, explicit.as_ptr(), null(), &mut acl) };
    if result != ERROR_SUCCESS || acl.is_null() {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(LocalAllocation::new(acl.cast()))
}

fn verify_acl_exact(acl: *mut ACL, expected: &[(&OwnedSid, u32)]) -> io::Result<()> {
    if acl.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows ACL is null",
        ));
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            acl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if size.AceCount != expected.len() as u32 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Windows ACL has {} ACEs; expected {}",
                size.AceCount,
                expected.len()
            ),
        ));
    }

    let mut seen = vec![false; expected.len()];
    for index in 0..size.AceCount {
        let mut raw_ace: *mut c_void = null_mut();
        if unsafe { GetAce(acl, index, &mut raw_ace) } == 0 || raw_ace.is_null() {
            return Err(io::Error::last_os_error());
        }
        let ace = raw_ace.cast::<ACCESS_ALLOWED_ACE>();
        let header = unsafe { &(*ace).Header };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8 || header.AceFlags != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows ACL contains an unexpected ACE type or inheritance flag",
            ));
        }
        let mask = unsafe { (*ace).Mask };
        let sid = unsafe { std::ptr::addr_of!((*ace).SidStart) as PSID };
        let mut matched = None;
        for (expected_index, (expected_sid, expected_mask)) in expected.iter().enumerate() {
            if unsafe { EqualSid(sid, expected_sid.as_psid()) } != 0 {
                if mask != *expected_mask {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Windows ACL contains an expected SID with the wrong access mask",
                    ));
                }
                matched = Some(expected_index);
                break;
            }
        }
        let Some(expected_index) = matched else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows ACL contains an unexpected SID",
            ));
        };
        if seen[expected_index] {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows ACL contains a duplicate SID",
            ));
        }
        seen[expected_index] = true;
    }
    if !seen.into_iter().all(|value| value) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows ACL is missing an expected SID",
        ));
    }
    Ok(())
}

pub fn apply_service_control_dacl(
    service_handle: windows_sys::Win32::System::Services::SC_HANDLE,
    authorized_user_sid: &str,
) -> io::Result<()> {
    validate_control_user_sid(authorized_user_sid)?;
    let system = OwnedSid::parse(SYSTEM_SID)?;
    let administrators = OwnedSid::parse(ADMINISTRATORS_SID)?;
    let authorized = OwnedSid::parse(authorized_user_sid)?;
    let acl = build_acl(&[
        (system.as_psid(), SERVICE_ALL_ACCESS),
        (administrators.as_psid(), SERVICE_ALL_ACCESS),
        (authorized.as_psid(), AUTHORIZED_SERVICE_ACCESS),
    ])?;
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    if unsafe {
        InitializeSecurityDescriptor(
            (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            SECURITY_DESCRIPTOR_REVISION,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        SetSecurityDescriptorDacl(
            (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            1,
            acl.as_ptr().cast(),
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        SetServiceObjectSecurity(
            service_handle,
            DACL_SECURITY_INFORMATION,
            (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    verify_service_control_dacl(service_handle, authorized_user_sid)
}

pub fn verify_service_control_dacl(
    service_handle: windows_sys::Win32::System::Services::SC_HANDLE,
    authorized_user_sid: &str,
) -> io::Result<()> {
    validate_control_user_sid(authorized_user_sid)?;
    let mut required = 0_u32;
    unsafe {
        QueryServiceObjectSecurity(
            service_handle,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            0,
            &mut required,
        );
    }
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_u8; required as usize];
    if unsafe {
        QueryServiceObjectSecurity(
            service_handle,
            DACL_SECURITY_INFORMATION,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut dacl: *mut ACL = null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    if unsafe {
        GetSecurityDescriptorDacl(
            buffer.as_mut_ptr().cast(),
            &mut present,
            &mut dacl,
            &mut defaulted,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if present == 0 || dacl.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows service control DACL is missing",
        ));
    }
    let system = OwnedSid::parse(SYSTEM_SID)?;
    let administrators = OwnedSid::parse(ADMINISTRATORS_SID)?;
    let authorized = OwnedSid::parse(authorized_user_sid)?;
    verify_acl_exact(
        dacl,
        &[
            (&system, SERVICE_ALL_ACCESS),
            (&administrators, SERVICE_ALL_ACCESS),
            (&authorized, AUTHORIZED_SERVICE_ACCESS),
        ],
    )
}

pub struct PipeSecurityAttributes {
    _acl: LocalAllocation,
    descriptor: Box<SECURITY_DESCRIPTOR>,
    attributes: SECURITY_ATTRIBUTES,
}

impl PipeSecurityAttributes {
    pub fn new(authorized_user_sid: &str, service_sid: &str) -> io::Result<Self> {
        validate_control_user_sid(authorized_user_sid)?;
        if !service_sid
            .to_ascii_uppercase()
            .starts_with(SERVICE_SID_PREFIX)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows service SID has an unexpected authority",
            ));
        }
        let system = OwnedSid::parse(SYSTEM_SID)?;
        let administrators = OwnedSid::parse(ADMINISTRATORS_SID)?;
        let service = OwnedSid::parse(service_sid)?;
        let authorized = OwnedSid::parse(authorized_user_sid)?;
        let user_mask = FILE_GENERIC_READ | FILE_GENERIC_WRITE;
        let acl = build_acl(&[
            (system.as_psid(), FILE_ALL_ACCESS),
            (administrators.as_psid(), FILE_ALL_ACCESS),
            (service.as_psid(), FILE_ALL_ACCESS),
            (authorized.as_psid(), user_mask),
        ])?;
        verify_acl_exact(
            acl.as_ptr().cast(),
            &[
                (&system, FILE_ALL_ACCESS),
                (&administrators, FILE_ALL_ACCESS),
                (&service, FILE_ALL_ACCESS),
                (&authorized, user_mask),
            ],
        )?;
        let mut descriptor = Box::new(SECURITY_DESCRIPTOR::default());
        if unsafe {
            InitializeSecurityDescriptor(
                (&mut *descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                SECURITY_DESCRIPTOR_REVISION,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if unsafe {
            SetSecurityDescriptorDacl(
                (&mut *descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                1,
                acl.as_ptr().cast(),
                0,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&mut *descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            bInheritHandle: 0,
        };
        Ok(Self {
            _acl: acl,
            descriptor,
            attributes,
        })
    }

    pub fn as_mut_ptr(&mut self) -> *mut c_void {
        debug_assert!(!self.descriptor.Dacl.is_null());
        (&mut self.attributes as *mut SECURITY_ATTRIBUTES).cast()
    }
}

#[cfg(test)]
pub(crate) struct RestrictedCurrentUserImpersonation {
    _token: OwnedHandle,
}

#[cfg(test)]
impl Drop for RestrictedCurrentUserImpersonation {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Security::RevertToSelf();
        }
    }
}

#[cfg(test)]
pub(crate) fn impersonate_restricted_current_user() -> io::Result<RestrictedCurrentUserImpersonation>
{
    use windows_sys::Win32::Security::{
        CheckTokenMembership, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE,
        ImpersonateLoggedOnUser, SID_AND_ATTRIBUTES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
    };

    let administrators = OwnedSid::parse(ADMINISTRATORS_SID)?;
    let mut source: HANDLE = null_mut();
    let desired = TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE;
    if unsafe { OpenProcessToken(GetCurrentProcess(), desired, &mut source) } == 0
        || source.is_null()
    {
        return Err(io::Error::last_os_error());
    }
    let source = OwnedHandle(source);
    let disabled = [SID_AND_ATTRIBUTES {
        Sid: administrators.as_psid(),
        Attributes: 0,
    }];
    let mut restricted: HANDLE = null_mut();
    if unsafe {
        CreateRestrictedToken(
            source.0,
            DISABLE_MAX_PRIVILEGE,
            disabled.len() as u32,
            disabled.as_ptr(),
            0,
            null(),
            0,
            null(),
            &mut restricted,
        )
    } == 0
        || restricted.is_null()
    {
        return Err(io::Error::last_os_error());
    }
    let restricted = OwnedHandle(restricted);
    if unsafe { ImpersonateLoggedOnUser(restricted.0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut is_admin = 0;
    if unsafe { CheckTokenMembership(null_mut(), administrators.as_psid(), &mut is_admin) } == 0 {
        let error = io::Error::last_os_error();
        unsafe {
            windows_sys::Win32::Security::RevertToSelf();
        }
        return Err(error);
    }
    if is_admin != 0 {
        unsafe {
            windows_sys::Win32::Security::RevertToSelf();
        }
        return Err(io::Error::other(
            "restricted Windows token unexpectedly retained Administrators membership",
        ));
    }
    Ok(RestrictedCurrentUserImpersonation { _token: restricted })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_is_stable_and_not_a_service_identity() {
        let first = current_user_sid_string().unwrap();
        let second = current_user_sid_string().unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("S-1-"));
        validate_control_user_sid(&first).unwrap();
    }

    #[test]
    fn pipe_security_accepts_numeric_unregistered_service_sid() {
        let user = current_user_sid_string().unwrap();
        let mut security = PipeSecurityAttributes::new(&user, "S-1-5-80-1-2-3-4-5").unwrap();
        assert!(!security.as_mut_ptr().is_null());
    }

    #[test]
    fn control_user_rejects_privileged_machine_identities() {
        assert!(validate_control_user_sid(SYSTEM_SID).is_err());
        assert!(validate_control_user_sid(ADMINISTRATORS_SID).is_err());
        assert!(validate_control_user_sid("S-1-5-80-1-2-3-4-5").is_err());
    }
}
