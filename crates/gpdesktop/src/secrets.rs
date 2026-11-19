//! Password storage in the Windows Credential Manager (per user, generic
//! credentials named `GlobalProtect VPN:<username>@<portal>`).

use anyhow::bail;
use windows_sys::Win32::{
  Foundation::{ERROR_NOT_FOUND, GetLastError},
  Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
  },
};
use zeroize::Zeroizing;

use crate::config::APP_NAME;

fn wide(value: &str) -> Vec<u16> {
  value.encode_utf16().chain(Some(0)).collect()
}

fn target(portal: &str, username: &str) -> Vec<u16> {
  wide(&format!("{APP_NAME}:{}@{}", username.trim(), portal.trim()))
}

pub fn store_password(portal: &str, username: &str, password: &str) -> anyhow::Result<()> {
  let target = target(portal, username);
  let user = wide(username.trim());
  let blob = Zeroizing::new(password.as_bytes().to_vec());
  let credential = CREDENTIALW {
    Flags: 0,
    Type: CRED_TYPE_GENERIC,
    TargetName: target.as_ptr() as *mut u16,
    Comment: std::ptr::null_mut(),
    LastWritten: unsafe { std::mem::zeroed() },
    CredentialBlobSize: blob.len() as u32,
    CredentialBlob: blob.as_ptr() as *mut u8,
    Persist: CRED_PERSIST_LOCAL_MACHINE,
    AttributeCount: 0,
    Attributes: std::ptr::null_mut(),
    TargetAlias: std::ptr::null_mut(),
    UserName: user.as_ptr() as *mut u16,
  };
  if unsafe { CredWriteW(&credential, 0) } == 0 {
    bail!("Cannot save the password: {}", std::io::Error::last_os_error());
  }
  Ok(())
}

pub fn load_password(portal: &str, username: &str) -> anyhow::Result<Option<Zeroizing<String>>> {
  let target = target(portal, username);
  let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
  unsafe {
    if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) == 0 {
      if GetLastError() == ERROR_NOT_FOUND {
        return Ok(None);
      }
      bail!("Cannot read the saved password: {}", std::io::Error::last_os_error());
    }
    let c = &*credential;
    let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
    let password = Zeroizing::new(String::from_utf8_lossy(bytes).into_owned());
    CredFree(credential.cast());
    Ok(Some(password))
  }
}

pub fn delete_password(portal: &str, username: &str) -> anyhow::Result<()> {
  let target = target(portal, username);
  unsafe {
    if CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) == 0 && GetLastError() != ERROR_NOT_FOUND {
      bail!("Cannot delete the saved password: {}", std::io::Error::last_os_error());
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trips_a_password() {
    let portal = format!("test-{}.invalid", uuid::Uuid::new_v4());
    assert!(load_password(&portal, "alice").unwrap().is_none());
    store_password(&portal, "alice", "s3cr3t-ü").unwrap();
    assert_eq!(load_password(&portal, "alice").unwrap().unwrap().as_str(), "s3cr3t-ü");
    delete_password(&portal, "alice").unwrap();
    assert!(load_password(&portal, "alice").unwrap().is_none());
    delete_password(&portal, "alice").unwrap();
  }
}
