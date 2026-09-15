//! Private directory creation and validation through owned security descriptors.
use ::windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{Authorization::*, *},
        Storage::FileSystem::FILE_ALL_ACCESS,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{HSTRING, PWSTR},
};
use std::{fs::File, io, mem::size_of, os::windows::io::AsRawHandle};

fn win(error: ::windows::core::Error) -> io::Error {
    io::Error::other(error)
}
fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "private directory owner or DACL differs from the expected user and SYSTEM",
    )
}
struct Local(HLOCAL);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(Some(self.0));
        }
    }
}
struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Backup evidence only; this descriptor never authorizes access or elevation.
pub(super) fn capture_owner_group_dacl(file: &File) -> io::Result<String> {
    let information =
        OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            information,
            None,
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    }
    .ok()
    .map_err(win)?;
    let _allocation = Local(HLOCAL(descriptor.0));
    if unsafe { GetSecurityDescriptorLength(descriptor) } > 65_536 {
        return Err(crate::invalid(
            "recovery security descriptor exceeds bounds",
        ));
    }
    descriptor_sddl(descriptor)
}

fn descriptor_sddl(descriptor: PSECURITY_DESCRIPTOR) -> io::Result<String> {
    let information =
        OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut text = PWSTR::null();
    let mut length = 0;
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            information,
            &mut text,
            Some(&mut length),
        )
    }
    .map_err(win)?;
    let _text = Local(HLOCAL(text.0.cast()));
    if length == 0 || length > 65_536 {
        return Err(crate::invalid("recovery security text exceeds bounds"));
    }
    unsafe { text.to_string() }.map_err(io::Error::other)
}

/// Establish ownership on the still-empty replacement before the executor moves
/// the original. Keep its private DACL until publication in the final parent.
pub(super) fn prepare_owner_group(file: &File, sddl: &str) -> io::Result<()> {
    let descriptor = Descriptor::from_sddl(sddl)?;
    let mut owner = PSID::default();
    let mut group = PSID::default();
    let mut defaulted = ::windows::core::BOOL::default();
    let mut present = ::windows::core::BOOL::default();
    let mut dacl = std::ptr::null_mut();
    unsafe {
        GetSecurityDescriptorOwner(descriptor.pointer, &mut owner, &mut defaulted).map_err(win)?;
        GetSecurityDescriptorGroup(descriptor.pointer, &mut group, &mut defaulted).map_err(win)?;
        GetSecurityDescriptorDacl(descriptor.pointer, &mut present, &mut dacl, &mut defaulted)
            .map_err(win)?;
    }
    if owner.0.is_null() || group.0.is_null() || !present.as_bool() || dacl.is_null() {
        return Err(crate::invalid(
            "source ownership or DACL cannot be preserved",
        ));
    }
    unsafe {
        SetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION,
            Some(owner),
            Some(group),
            None,
            None,
        )
    }
    .ok()
    .map_err(win)?;
    let mut actual_owner = PSID::default();
    let mut actual_group = PSID::default();
    let mut actual = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION,
            Some(&mut actual_owner),
            Some(&mut actual_group),
            None,
            None,
            Some(&mut actual),
        )
    }
    .ok()
    .map_err(win)?;
    let _actual = Local(HLOCAL(actual.0));
    if actual_owner.0.is_null()
        || actual_group.0.is_null()
        || sid_text(actual_owner)? != sid_text(owner)?
        || sid_text(actual_group)? != sid_text(group)?
    {
        return Err(crate::invalid("replacement ownership was not retained"));
    }
    Ok(())
}

/// Apply saved permissions to the replacement at its final parent, where Windows
/// inheritance has the same context as the original. Failure is post-publication
/// uncertainty; callers must retain recovery material and must not replay.
pub(super) fn apply_owner_group_dacl(file: &File, sddl: &str) -> io::Result<()> {
    let descriptor = Descriptor::from_sddl(sddl)?;
    let mut owner = PSID::default();
    let mut group = PSID::default();
    let mut defaulted = ::windows::core::BOOL::default();
    let mut present = ::windows::core::BOOL::default();
    let mut dacl = std::ptr::null_mut();
    let mut control = 0;
    let mut revision = 0;
    unsafe {
        GetSecurityDescriptorOwner(descriptor.pointer, &mut owner, &mut defaulted).map_err(win)?;
        GetSecurityDescriptorGroup(descriptor.pointer, &mut group, &mut defaulted).map_err(win)?;
        GetSecurityDescriptorDacl(descriptor.pointer, &mut present, &mut dacl, &mut defaulted)
            .map_err(win)?;
        GetSecurityDescriptorControl(descriptor.pointer, &mut control, &mut revision)
            .map_err(win)?;
    }
    if !present.as_bool() || dacl.is_null() {
        return Err(crate::invalid(
            "replacement requires an explicit source DACL",
        ));
    }
    let inheritance = if control & SE_DACL_PROTECTED.0 != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    unsafe {
        SetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | GROUP_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | inheritance,
            Some(owner),
            Some(group),
            Some(dacl),
            None,
        )
    }
    .ok()
    .map_err(win)?;
    if !replacement_permissions_preserved(sddl, &capture_owner_group_dacl(file)?)? {
        return Err(crate::invalid(
            "replacement permissions differ from the original",
        ));
    }
    Ok(())
}

/// SetSecurityInfo can add AI to an already protected DACL. Protection still
/// disables inheritance, so accept only this one-way bookkeeping change while
/// comparing every owner/group/ACE field. Unprotected DACLs stay exact: their
/// auto-inheritance state can affect subsequent permission propagation.
fn replacement_permissions_preserved(expected: &str, actual: &str) -> io::Result<bool> {
    if expected == actual {
        return Ok(true);
    }
    let expected = Descriptor::from_sddl(expected)?;
    let actual = Descriptor::from_sddl(actual)?;
    let (mut expected_control, mut actual_control) = (0, 0);
    let (mut expected_revision, mut actual_revision) = (0, 0);
    unsafe {
        GetSecurityDescriptorControl(
            expected.pointer,
            &mut expected_control,
            &mut expected_revision,
        )
        .map_err(win)?;
        GetSecurityDescriptorControl(actual.pointer, &mut actual_control, &mut actual_revision)
            .map_err(win)?;
    }
    if expected_control & SE_DACL_PROTECTED.0 == 0
        || expected_control & SE_DACL_AUTO_INHERITED.0 != 0
        || actual_control != (expected_control | SE_DACL_AUTO_INHERITED.0)
        || expected_revision != actual_revision
    {
        return Ok(false);
    }
    // Normalize only our allocated comparison descriptor, never the file ACL.
    unsafe {
        SetSecurityDescriptorControl(
            actual.pointer,
            SE_DACL_AUTO_INHERITED,
            SECURITY_DESCRIPTOR_CONTROL(0),
        )
    }
    .map_err(win)?;
    Ok(descriptor_sddl(expected.pointer)? == descriptor_sddl(actual.pointer)?)
}

fn sid_text(sid: PSID) -> io::Result<String> {
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut text) }.map_err(win)?;
    let _allocation = Local(HLOCAL(text.0.cast()));
    unsafe { text.to_string() }.map_err(io::Error::other)
}

#[cfg(test)]
mod replacement_tests {
    use super::replacement_permissions_preserved;

    #[test]
    fn protected_dacl_allows_only_added_auto_inherited_bookkeeping() {
        let original = "O:SYG:SYD:P(A;;FA;;;SY)(A;;FR;;;BA)";
        let inherited = "O:SYG:SYD:PAI(A;;FA;;;SY)(A;;FR;;;BA)";
        assert!(replacement_permissions_preserved(original, original).unwrap());
        assert!(replacement_permissions_preserved(original, inherited).unwrap());
        assert!(!replacement_permissions_preserved(inherited, original).unwrap());
        for changed in [
            "O:BAG:SYD:PAI(A;;FA;;;SY)(A;;FR;;;BA)",
            "O:SYG:BAD:PAI(A;;FA;;;SY)(A;;FR;;;BA)",
            "O:SYG:SYD:AI(A;;FA;;;SY)(A;;FR;;;BA)",
            "O:SYG:SYD:PAI(A;;FA;;;SY)(A;;FA;;;BA)",
            "O:SYG:SYD:PAI(A;;FA;;;SY)(D;;FR;;;BA)",
            "O:SYG:SYD:PAI(A;;FR;;;BA)(A;;FA;;;SY)",
            "O:SYG:SYD:PAI(A;;FA;;;SY)(A;ID;FR;;;BA)",
        ] {
            assert!(
                !replacement_permissions_preserved(original, changed).unwrap(),
                "{changed}"
            );
        }
        assert!(
            !replacement_permissions_preserved(
                "O:SYG:SYD:(A;;FA;;;SY)",
                "O:SYG:SYD:AI(A;;FA;;;SY)",
            )
            .unwrap()
        );
    }
}

pub(super) fn current_user() -> io::Result<String> {
    process_user(unsafe { GetCurrentProcess() })
}

pub(super) fn process_user(process: HANDLE) -> io::Result<String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.map_err(win)?;
    let token = Token(token);
    let mut bytes = 0;
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut bytes) };
    if !(size_of::<TOKEN_USER>() as u32..=65_536).contains(&bytes) {
        return Err(denied());
    }
    let mut data = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(data.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )
    }
    .map_err(win)?;
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    sid_text(user.User.Sid)
}

pub(super) struct Descriptor {
    pub(super) pointer: PSECURITY_DESCRIPTOR,
    _allocation: Local,
}
impl Descriptor {
    pub(super) fn from_sddl(sddl: &str) -> io::Result<Self> {
        let mut pointer = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut pointer,
                None,
            )
        }
        .map_err(win)?;
        Ok(Self {
            pointer,
            _allocation: Local(HLOCAL(pointer.0)),
        })
    }
}

pub(super) fn validate_private(file: &File, user: &str) -> io::Result<()> {
    validate_permissions(
        file,
        user,
        (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE).0 as u8,
    )
}

pub(super) fn validate_private_file(file: &File, user: &str) -> io::Result<()> {
    validate_permissions(file, user, 0)?;
    let streams = super::stream_inventory(file, crate::MAX_LEDGER_BYTES)?;
    if streams.streams.len() != 1 || streams.streams[0].name != "::$DATA" {
        return Err(denied());
    }
    Ok(())
}

fn validate_permissions(file: &File, user: &str, flags: u8) -> io::Result<()> {
    let mut owner = PSID::default();
    let mut dacl = std::ptr::null_mut::<ACL>();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut descriptor),
        )
    }
    .ok()
    .map_err(win)?;
    let _allocation = Local(HLOCAL(descriptor.0));
    let mut control = 0;
    let mut revision = 0;
    unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) }
        .map_err(win)?;
    if owner.0.is_null()
        || sid_text(owner)? != user
        || dacl.is_null()
        || control & SE_DACL_PROTECTED.0 == 0
    {
        return Err(denied());
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    }
    .map_err(win)?;
    if info.AceCount != 2 {
        return Err(denied());
    }
    let mut principals = Vec::new();
    for index in 0..info.AceCount {
        let mut pointer = std::ptr::null_mut();
        unsafe { GetAce(dacl, index, &mut pointer) }.map_err(win)?;
        let header = unsafe { &*pointer.cast::<ACE_HEADER>() };
        if header.AceType != 0
            || header.AceFlags != flags
            || (header.AceSize as usize) < std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart) + 8
        {
            return Err(denied());
        }
        let ace = unsafe { &*pointer.cast::<ACCESS_ALLOWED_ACE>() };
        if ace.Mask != FILE_ALL_ACCESS.0 {
            return Err(denied());
        }
        let sid = PSID(std::ptr::addr_of!(ace.SidStart).cast_mut().cast());
        if unsafe { GetLengthSid(sid) } as usize
            > header.AceSize as usize - std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart)
        {
            return Err(denied());
        }
        principals.push(sid_text(sid)?);
    }
    principals.sort();
    let mut expected = vec![user.to_owned(), "S-1-5-18".into()];
    expected.sort();
    if principals != expected {
        return Err(denied());
    }
    Ok(())
}
