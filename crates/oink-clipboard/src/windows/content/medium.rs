//! COM handles never cross the wire. Each medium has a byte reconstruction
//! contract, and the receiver creates a fresh locally owned STGMEDIUM.
use super::*;
use std::mem::{ManuallyDrop, size_of};
use windows::Win32::{
    Foundation::{GlobalFree, HGLOBAL},
    Graphics::Gdi::*,
    System::{Com::StructuredStorage::*, Memory::*},
};

pub(super) fn kind(tymed: u32, cf: u16) -> Option<Medium> {
    Some(match tymed {
        n if n == TYMED_HGLOBAL.0 as u32 => Medium::HGlobal,
        n if n == TYMED_ISTREAM.0 as u32 => Medium::Stream,
        n if n == TYMED_ISTORAGE.0 as u32 => Medium::Storage,
        n if n == TYMED_GDI.0 as u32 && cf == 9 => Medium::Palette,
        n if n == TYMED_GDI.0 as u32 => Medium::Bitmap,
        n if n == TYMED_MFPICT.0 as u32 => Medium::Metafile,
        n if n == TYMED_ENHMF.0 as u32 => Medium::EnhancedMetafile,
        n if n == TYMED_FILE.0 as u32 => Medium::File,
        _ => return None,
    })
}
pub(super) fn tymed(medium: Medium) -> u32 {
    match medium {
        Medium::Bytes | Medium::HGlobal => TYMED_HGLOBAL,
        Medium::Stream => TYMED_ISTREAM,
        Medium::Storage => TYMED_ISTORAGE,
        Medium::Bitmap | Medium::Palette => TYMED_GDI,
        Medium::Metafile => TYMED_MFPICT,
        Medium::EnhancedMetafile => TYMED_ENHMF,
        Medium::File => TYMED_FILE,
    }
    .0 as u32
}
fn checked(n: usize) -> io::Result<usize> {
    if n as u64 > REPRESENTATION_BYTES {
        Err(content::unavailable(Unavailable::LimitExceeded))
    } else {
        Ok(n)
    }
}
fn failed() -> io::Error {
    content::unavailable(Unavailable::ReadFailed)
}
fn win<T>(value: windows::core::Result<T>) -> io::Result<T> {
    value.map_err(|_| failed())
}

pub(super) fn global(h: HGLOBAL) -> io::Result<Vec<u8>> {
    // SAFETY: caller owns a live HGLOBAL STGMEDIUM. Size is bounded before copying;
    // GlobalUnlock occurs on the same thread before the medium is released.
    unsafe {
        let n = checked(GlobalSize(h))?;
        if n == 0 {
            return Ok(Vec::new());
        }
        let p = GlobalLock(h);
        if p.is_null() {
            return Err(failed());
        }
        let bytes = std::slice::from_raw_parts(p.cast::<u8>(), n).to_vec();
        let _ = GlobalUnlock(h);
        Ok(bytes)
    }
}
fn allocate(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // SAFETY: new movable memory is exclusively ours until passed to OLE. The
    // caller transfers it only on success; failures free it here.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))?;
        let p = GlobalLock(h);
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return Err(STG_E_READFAULT.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast(), bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

pub(super) fn serialize(medium: &STGMEDIUM, kind: Medium) -> io::Result<Vec<u8>> {
    // SAFETY: GetData supplied this medium in the current COM apartment. All
    // union members match kind/tymed; temporary COM/GDI resources are released
    // before returning and only owned byte vectors leave the apartment.
    unsafe {
        match kind {
            Medium::HGlobal | Medium::Bytes => global(medium.u.hGlobal),
            Medium::File => {
                let name = medium.u.lpszFileName.to_string().map_err(|_| failed())?;
                content::read_all(&mut std::fs::File::open(name)?)
            }
            Medium::Stream => {
                let stream = medium.u.pstm.as_ref().ok_or_else(failed)?;
                let mut saved = 0;
                let seekable = stream.Seek(0, STREAM_SEEK_CUR, Some(&mut saved)).is_ok();
                if seekable {
                    win(stream.Seek(0, STREAM_SEEK_SET, None))?;
                }
                let result = (|| {
                    let mut out = Vec::new();
                    loop {
                        let mut bytes = vec![0u8; 1024 * 1024];
                        let mut n = 0;
                        stream
                            .Read(bytes.as_mut_ptr().cast(), bytes.len() as u32, Some(&mut n))
                            .ok()
                            .map_err(|_| failed())?;
                        checked(out.len().checked_add(n as usize).ok_or_else(failed)?)?;
                        out.extend_from_slice(&bytes[..n as usize]);
                        if n == 0 {
                            return Ok(out);
                        }
                    }
                })();
                if seekable {
                    let _ = stream.Seek(saved as i64, STREAM_SEEK_SET, None);
                }
                result
            }
            Medium::Storage => {
                let source = medium.u.pstg.as_ref().ok_or_else(failed)?;
                let bytes = win(CreateILockBytesOnHGlobal(None, true))?;
                let exceeded = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let bounded: ILockBytes = BoundedLock {
                    inner: bytes.clone(),
                    limit: REPRESENTATION_BYTES,
                    exceeded: exceeded.clone(),
                }
                .into();
                let copy = win(StgCreateDocfileOnILockBytes(
                    &bounded,
                    STGM_CREATE | STGM_READWRITE | STGM_SHARE_EXCLUSIVE,
                    0,
                ))?;
                let copied = source.CopyTo(None, None, &copy);
                if exceeded.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(content::unavailable(Unavailable::LimitExceeded));
                }
                win(copied)?;
                let mut stat = STATSTG::default();
                win(source.Stat(&mut stat, STATFLAG_NONAME.0 as u32))?;
                win(copy.SetClass(&stat.clsid))?;
                win(copy.SetStateBits(stat.grfStateBits, u32::MAX))?;
                win(copy.Commit(0))?;
                drop(copy);
                global(win(GetHGlobalFromILockBytes(&bytes))?)
            }
            Medium::EnhancedMetafile => {
                let h = medium.u.hEnhMetaFile;
                let n = checked(GetEnhMetaFileBits(h, None) as usize)?;
                let mut bytes = vec![0; n];
                if n == 0 || GetEnhMetaFileBits(h, Some(&mut bytes)) as usize != n {
                    return Err(failed());
                }
                Ok(bytes)
            }
            Medium::Metafile => {
                let h = HGLOBAL(medium.u.hMetaFilePict);
                if GlobalSize(h) < size_of::<METAFILEPICT>() {
                    return Err(failed());
                }
                let ptr = GlobalLock(h).cast::<METAFILEPICT>();
                if ptr.is_null() {
                    return Err(failed());
                }
                let pict = *ptr;
                let _ = GlobalUnlock(h);
                let n = checked(GetMetaFileBitsEx(pict.hMF, 0, None) as usize)?;
                let mut bytes = vec![0; n + 12];
                bytes[..4].copy_from_slice(&pict.mm.to_le_bytes());
                bytes[4..8].copy_from_slice(&pict.xExt.to_le_bytes());
                bytes[8..12].copy_from_slice(&pict.yExt.to_le_bytes());
                if n == 0 || GetMetaFileBitsEx(pict.hMF, n as u32, Some(bytes[12..].as_mut_ptr().cast())) as usize != n
                {
                    return Err(failed());
                }
                checked(bytes.len())?;
                Ok(bytes)
            }
            Medium::Palette => {
                let h = HPALETTE(medium.u.hBitmap.0);
                let mut count = 0u16;
                if GetObjectW(HGDIOBJ(h.0), 2, Some((&mut count as *mut u16).cast())) != 2 {
                    return Err(failed());
                }
                let mut entries = vec![PALETTEENTRY::default(); count as usize];
                if GetPaletteEntries(h, 0, Some(&mut entries)) != count as u32 {
                    return Err(failed());
                }
                let mut out = Vec::with_capacity(4 + entries.len() * 4);
                out.extend_from_slice(&0x300u16.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
                for p in entries {
                    out.extend_from_slice(&[p.peRed, p.peGreen, p.peBlue, p.peFlags]);
                }
                Ok(out)
            }
            Medium::Bitmap => {
                let mut bitmap = BITMAP::default();
                if GetObjectW(
                    HGDIOBJ(medium.u.hBitmap.0),
                    size_of::<BITMAP>() as i32,
                    Some((&mut bitmap as *mut BITMAP).cast()),
                ) == 0
                {
                    return Err(failed());
                }
                let w = usize::try_from(bitmap.bmWidth).map_err(|_| failed())?;
                let h = usize::try_from(bitmap.bmHeight).map_err(|_| failed())?;
                let n = checked(
                    w.checked_mul(h)
                        .and_then(|n| n.checked_mul(4))
                        .and_then(|n| n.checked_add(8))
                        .ok_or_else(failed)?,
                )?;
                let mut out = vec![0; n];
                out[..4].copy_from_slice(&bitmap.bmWidth.to_le_bytes());
                out[4..8].copy_from_slice(&bitmap.bmHeight.to_le_bytes());
                let mut info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: bitmap.bmWidth,
                        biHeight: bitmap.bmHeight,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let dc = CreateCompatibleDC(None);
                if dc.is_invalid() {
                    return Err(failed());
                }
                let rows = GetDIBits(
                    dc,
                    medium.u.hBitmap,
                    0,
                    h as u32,
                    Some(out[8..].as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS,
                );
                let _ = DeleteDC(dc);
                if rows != h as i32 {
                    return Err(failed());
                }
                Ok(out)
            }
        }
    }
}

pub(super) fn restore(kind: Medium, bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    // SAFETY: only validated bytes construct fresh native resources. Ownership
    // transfers in the returned STGMEDIUM; ReleaseStgMedium frees those resources.
    unsafe {
        let u = match kind {
            Medium::Bytes | Medium::HGlobal => STGMEDIUM_0 {
                hGlobal: allocate(bytes)?,
            },
            Medium::Stream => {
                let global = allocate(bytes)?;
                let stream = match CreateStreamOnHGlobal(global, true) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = GlobalFree(Some(global));
                        return Err(e);
                    }
                };
                stream.SetSize(bytes.len() as u64)?;
                STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                }
            }
            Medium::Storage => {
                let global = allocate(bytes)?;
                let lock = match CreateILockBytesOnHGlobal(Some(global), true) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = GlobalFree(Some(global));
                        return Err(e);
                    }
                };
                let storage = StgOpenStorageOnILockBytes(&lock, None, STGM_READ | STGM_SHARE_EXCLUSIVE, None, None)?;
                STGMEDIUM_0 {
                    pstg: ManuallyDrop::new(Some(storage)),
                }
            }
            Medium::EnhancedMetafile => {
                let h = SetEnhMetaFileBits(bytes);
                if h.is_invalid() {
                    return Err(STG_E_READFAULT.into());
                }
                STGMEDIUM_0 { hEnhMetaFile: h }
            }
            Medium::Metafile => {
                if bytes.len() < 12 {
                    return Err(STG_E_READFAULT.into());
                }
                let h = SetMetaFileBitsEx(&bytes[12..]);
                if h.is_invalid() {
                    return Err(STG_E_READFAULT.into());
                }
                let pict = METAFILEPICT {
                    mm: i32::from_le_bytes(bytes[..4].try_into().unwrap()),
                    xExt: i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                    yExt: i32::from_le_bytes(bytes[8..12].try_into().unwrap()),
                    hMF: h,
                };
                let raw =
                    std::slice::from_raw_parts((&pict as *const METAFILEPICT).cast::<u8>(), size_of::<METAFILEPICT>());
                match allocate(raw) {
                    Ok(h) => STGMEDIUM_0 { hMetaFilePict: h.0 },
                    Err(e) => {
                        let _ = DeleteMetaFile(h);
                        return Err(e);
                    }
                }
            }
            Medium::Palette => {
                if bytes.len() < 4 || bytes.len() != 4 + u16::from_le_bytes([bytes[2], bytes[3]]) as usize * 4 {
                    return Err(STG_E_READFAULT.into());
                }
                // Vec<u16> supplies LOGPALETTE alignment; the buffer has the
                // exact variable-length palette entry array expected by GDI.
                let aligned: Vec<u16> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .collect();
                let h = CreatePalette(aligned.as_ptr().cast());
                if h.is_invalid() {
                    return Err(STG_E_READFAULT.into());
                }
                STGMEDIUM_0 { hBitmap: HBITMAP(h.0) }
            }
            Medium::Bitmap => {
                if bytes.len() < 8 {
                    return Err(STG_E_READFAULT.into());
                }
                let w = i32::from_le_bytes(bytes[..4].try_into().unwrap());
                let h = i32::from_le_bytes(bytes[4..8].try_into().unwrap());
                if w <= 0 || h <= 0 || (w as u64) * (h as u64) * 4 != (bytes.len() - 8) as u64 {
                    return Err(STG_E_READFAULT.into());
                }
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w,
                        biHeight: h,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut ptr = std::ptr::null_mut();
                let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut ptr, None, 0)?;
                if ptr.is_null() {
                    let _ = DeleteObject(HGDIOBJ(bitmap.0));
                    return Err(STG_E_READFAULT.into());
                }
                std::ptr::copy_nonoverlapping(bytes[8..].as_ptr(), ptr.cast(), bytes.len() - 8);
                STGMEDIUM_0 { hBitmap: bitmap }
            }
            Medium::File => return Err(DV_E_TYMED.into()),
        };
        Ok(STGMEDIUM {
            tymed: tymed(kind),
            u,
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}

// IStorage::CopyTo can write sparse/high-offset streams. Bound its backing store
// before allocation, not after converting an arbitrarily large compound file.
#[windows::core::implement(ILockBytes, Agile = false)]
struct BoundedLock {
    inner: ILockBytes,
    limit: u64,
    exceeded: Arc<std::sync::atomic::AtomicBool>,
}
impl BoundedLock {
    fn check(&self, end: Option<u64>) -> windows::core::Result<()> {
        if end.is_some_and(|n| n <= self.limit) {
            return Ok(());
        }
        self.exceeded.store(true, std::sync::atomic::Ordering::Release);
        Err(windows::Win32::Foundation::STG_E_MEDIUMFULL.into())
    }
}
impl ILockBytes_Impl for BoundedLock_Impl {
    fn ReadAt(
        &self,
        offset: u64,
        data: *mut core::ffi::c_void,
        count: u32,
        read: *mut u32,
    ) -> windows::core::Result<()> {
        // SAFETY: unchanged COM-provided pointers forwarded to the native store.
        unsafe { self.inner.ReadAt(offset, data, count, Some(read)) }
    }
    fn WriteAt(
        &self,
        offset: u64,
        data: *const core::ffi::c_void,
        count: u32,
        written: *mut u32,
    ) -> windows::core::Result<()> {
        if let Err(error) = self.check(offset.checked_add(count as u64)) {
            if !written.is_null() {
                // SAFETY: optional COM out pointer, valid for this invocation.
                unsafe {
                    *written = 0;
                }
            }
            return Err(error);
        }
        // SAFETY: unchanged COM pointers; offset/count now fit the budget.
        unsafe { self.inner.WriteAt(offset, data, count, Some(written)) }
    }
    fn SetSize(&self, size: u64) -> windows::core::Result<()> {
        self.check(Some(size))?;
        // SAFETY: apartment-owned native store and validated length.
        unsafe { self.inner.SetSize(size) }
    }
    fn Flush(&self) -> windows::core::Result<()> {
        // SAFETY: apartment-owned native store.
        unsafe { self.inner.Flush() }
    }
    fn LockRegion(&self, offset: u64, count: u64, kind: u32) -> windows::core::Result<()> {
        // SAFETY: no pointers; native store validates lock ranges.
        unsafe { self.inner.LockRegion(offset, count, kind) }
    }
    fn UnlockRegion(&self, offset: u64, count: u64, kind: u32) -> windows::core::Result<()> {
        // SAFETY: no pointers; native store validates lock ranges.
        unsafe { self.inner.UnlockRegion(offset, count, kind) }
    }
    fn Stat(&self, stat: *mut STATSTG, flags: u32) -> windows::core::Result<()> {
        // SAFETY: unchanged COM out pointer forwarded to the native store.
        unsafe { self.inner.Stat(stat, flags) }
    }
}
