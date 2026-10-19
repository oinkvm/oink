use super::super::wide;
use super::protected;
use oink_update::{Error, feed::hex, trust::Roots};
use sha2::{Digest, Sha256};
use std::{os::windows::io::AsRawHandle, path::Path, ptr};
use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    Security::{Cryptography::*, WinTrust::*},
};

pub(super) fn verify(path: &Path, roots: &Roots) -> Result<(), Error> {
    let file = protected::open(path).map_err(|_| Error::Storage)?;
    let name = wide(path.to_str().ok_or(Error::Storage)?);
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: name.as_ptr(),
        hFile: file.as_raw_handle(),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_WHOLECHAIN,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: all backing structs, file handle and wide path remain live; no UI is permitted.
    let result = unsafe { WinVerifyTrust(INVALID_HANDLE_VALUE, &mut action, ptr::addr_of_mut!(data).cast()) };
    let pinned = if result == 0 {
        pin(data.hWVTStateData, roots)
    } else {
        Err(Error::Signature)
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: close exactly the provider state created above, on every return path.
    unsafe {
        WinVerifyTrust(INVALID_HANDLE_VALUE, &mut action, ptr::addr_of_mut!(data).cast());
    }
    pinned
}
fn pin(state: windows_sys::Win32::Foundation::HANDLE, roots: &Roots) -> Result<(), Error> {
    // SAFETY: state belongs to a successful WinVerifyTrust call and remains open
    // throughout these read-only provider/certificate queries.
    unsafe {
        let provider = WTHelperProvDataFromStateData(state);
        if provider.is_null() {
            return Err(Error::Signature);
        }
        let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
        if signer.is_null() {
            return Err(Error::Signature);
        }
        let certificate = WTHelperGetProvCertFromChain(signer, 0);
        if certificate.is_null() || (*certificate).pCert.is_null() {
            return Err(Error::Signature);
        }
        let info = (*(*certificate).pCert).pCertInfo;
        if info.is_null() {
            return Err(Error::Signature);
        }
        let key = ptr::addr_of!((*info).SubjectPublicKeyInfo).cast();
        let mut length = 0;
        if CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            X509_PUBLIC_KEY_INFO,
            key,
            0,
            ptr::null(),
            ptr::null_mut(),
            &mut length,
        ) == 0
            || length > 16384
        {
            return Err(Error::Signature);
        }
        let mut encoded = vec![0u8; length as usize];
        if CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            X509_PUBLIC_KEY_INFO,
            key,
            0,
            ptr::null(),
            encoded.as_mut_ptr().cast(),
            &mut length,
        ) == 0
        {
            return Err(Error::Signature);
        }
        encoded.truncate(length as usize);
        let hash = hex(&Sha256::digest(&encoded));
        if !roots.signer_spki_sha256.contains(&hash) {
            return Err(Error::Signature);
        }
        Ok(())
    }
}
