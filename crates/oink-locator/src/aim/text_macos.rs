//! macOS：用 CoreText 把一行字画进 8 位灰度位图。
//!
//! 字体是系统界面字体（`CTFontCreateUIFontForLanguage`，语言 zh-Hans）：拉丁与数字是 SF，中文按系统的
//! 回落顺序落到苹方（简体字形）。CoreText 与位图上下文都可以在任意线程使用。

use std::ffi::c_void;
use std::sync::OnceLock;

use super::{Mask, TextRaster};

type Ref = *const c_void;

#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    static kCFBooleanTrue: Ref;
    fn CFStringCreateWithBytes(alloc: Ref, bytes: *const u8, len: isize, encoding: u32, external: u8) -> Ref;
    fn CFDictionaryCreate(
        alloc: Ref,
        keys: *const Ref,
        values: *const Ref,
        count: isize,
        key_callbacks: *const Opaque,
        value_callbacks: *const Opaque,
    ) -> Ref;
    fn CFAttributedStringCreate(alloc: Ref, string: Ref, attributes: Ref) -> Ref;
    fn CFRelease(object: Ref);
    fn CFNumberCreate(alloc: Ref, kind: isize, value: *const f64) -> Ref;
}

#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    static kCTFontAttributeName: Ref;
    static kCTFontTraitsAttribute: Ref;
    static kCTFontWeightTrait: Ref;
    fn CTFontDescriptorCreateWithAttributes(attributes: Ref) -> Ref;
    fn CTFontCreateCopyWithAttributes(font: Ref, size: f64, matrix: Ref, descriptor: Ref) -> Ref;
    static kCTForegroundColorFromContextAttributeName: Ref;
    fn CTFontCreateUIFontForLanguage(ui_type: u32, size: f64, language: Ref) -> Ref;
    fn CTFontGetCapHeight(font: Ref) -> f64;
    fn CTLineCreateWithAttributedString(string: Ref) -> Ref;
    fn CTLineGetTypographicBounds(line: Ref, ascent: *mut f64, descent: *mut f64, leading: *mut f64) -> f64;
    fn CTLineDraw(line: Ref, context: Ref);
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorSpaceCreateDeviceGray() -> Ref;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: Ref,
        bitmap_info: u32,
    ) -> Ref;
    fn CGContextSetGrayFillColor(context: Ref, gray: f64, alpha: f64);
    fn CGContextSetTextPosition(context: Ref, x: f64, y: f64);
}

const UTF8: u32 = 0x0800_0100;
/// `kCTFontUIFontSystem` / `kCTFontUIFontEmphasizedSystem`。
const UI_SYSTEM: u32 = 2;
const UI_EMPHASIZED: u32 = 3;

/// 持有的 CoreFoundation 对象，离开作用域时释放。
struct Owned(Ref);

impl Owned {
    fn new(r: Ref) -> Option<Self> {
        (!r.is_null()).then_some(Self(r))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: 非空、由 Create 函数返回、只释放一次。
        unsafe { CFRelease(self.0) };
    }
}

fn cf_string(s: &str) -> Option<Owned> {
    // SAFETY: 指针与长度来自同一个 &str；CoreFoundation 复制内容。
    Owned::new(unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, UTF8, 0) })
}

fn font(bold: bool, size: f64, medium: bool) -> Option<Owned> {
    let language = cf_string("zh-Hans")?;
    let kind = if bold { UI_EMPHASIZED } else { UI_SYSTEM };
    // SAFETY: 系统定义的界面字体类型；语言是有效的 CFString。
    let base = Owned::new(unsafe { CTFontCreateUIFontForLanguage(kind, size, language.0) })?;
    if !medium || bold {
        return Some(base);
    }
    let weight = 0.23_f64; // CoreText's system medium weight (CSS 500).
    // SAFETY: CFNumber copies the f64; nested dictionaries retain their CF values.
    unsafe {
        let weight = Owned::new(CFNumberCreate(std::ptr::null(), 6, &weight))?;
        let traits = Owned::new(CFDictionaryCreate(
            std::ptr::null(),
            &kCTFontWeightTrait,
            &weight.0,
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        ))?;
        let attrs = Owned::new(CFDictionaryCreate(
            std::ptr::null(),
            &kCTFontTraitsAttribute,
            &traits.0,
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        ))?;
        let descriptor = Owned::new(CTFontDescriptorCreateWithAttributes(attrs.0))?;
        Owned::new(CTFontCreateCopyWithAttributes(
            base.0,
            size,
            std::ptr::null(),
            descriptor.0,
        ))
    }
}

/// cap 高与字号的比例（按字重各算一次）。
fn cap_ratio(bold: bool, medium: bool) -> Option<f64> {
    static RATIO: [OnceLock<Option<f64>>; 4] = [const { OnceLock::new() }; 4];
    *RATIO[usize::from(bold) + usize::from(medium) * 2].get_or_init(|| {
        let f = font(bold, 100.0, medium)?;
        // SAFETY: 有效的 CTFont。
        let cap = unsafe { CTFontGetCapHeight(f.0) };
        (cap > 10.0).then_some(cap / 100.0)
    })
}

#[derive(Debug)]
pub(super) struct SystemText {
    medium: bool,
}

impl SystemText {
    pub(super) fn medium() -> Option<Self> {
        cap_ratio(false, true).map(|_| Self { medium: true })
    }
    pub(super) fn new() -> Option<Self> {
        (cap_ratio(false, false).is_some() && cap_ratio(true, false).is_some()).then_some(Self { medium: false })
    }
}

impl TextRaster for SystemText {
    fn line(&self, text: &str, cap: f64, bold: bool) -> Mask {
        render(text, cap, bold, self.medium).unwrap_or_default()
    }
}

fn render(text: &str, cap: f64, bold: bool, medium: bool) -> Option<Mask> {
    let font = font(bold, cap.max(1.0) / cap_ratio(bold, medium)?, medium)?;
    let string = cf_string(text)?;
    // SAFETY: 两个系统定义的属性键；值分别是有效的 CTFont 与 kCFBooleanTrue。字典按 CF 类型的回调持有它们。
    let attributes = Owned::new(unsafe {
        let keys = [kCTFontAttributeName, kCTForegroundColorFromContextAttributeName];
        let values = [font.0, kCFBooleanTrue];
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            2,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    })?;
    // SAFETY: 有效的字符串与属性字典。
    let attributed = Owned::new(unsafe { CFAttributedStringCreate(std::ptr::null(), string.0, attributes.0) })?;
    // SAFETY: 有效的属性字符串。
    let line = Owned::new(unsafe { CTLineCreateWithAttributedString(attributed.0) })?;
    let (mut ascent, mut descent, mut leading) = (0.0, 0.0, 0.0);
    // SAFETY: 输出参数是本地变量。
    let width = unsafe { CTLineGetTypographicBounds(line.0, &mut ascent, &mut descent, &mut leading) };
    // 左右各留一点：斜体、粗体的墨迹可能略超出排版宽度。
    let pad = (cap * 0.08).ceil().max(1.0);
    let w = (width + pad * 2.0).ceil().max(1.0) as usize;
    let h = (ascent.ceil() + descent.ceil()).max(1.0) as usize;
    let mut alpha = vec![0u8; w * h];
    // SAFETY: 缓冲区 w × h 字节、每行 w 字节，与上下文的参数一致；上下文在缓冲区之前释放。
    // 灰度、没有 alpha 通道（kCGImageAlphaNone = 0）：黑底上画白字，像素值就是覆盖率。
    unsafe {
        let space = Owned::new(CGColorSpaceCreateDeviceGray())?;
        let context = Owned::new(CGBitmapContextCreate(alpha.as_mut_ptr().cast(), w, h, 8, w, space.0, 0))?;
        CGContextSetGrayFillColor(context.0, 1.0, 1.0);
        // 位图上下文的原点在左下角：基线离底边一个 descent。
        CGContextSetTextPosition(context.0, pad, descent.ceil());
        CTLineDraw(line.0, context.0);
    }
    Some(Mask {
        w,
        h,
        alpha,
        cap_top: ascent.ceil() - cap,
        advance: width,
        left: pad,
    })
}
