//! macOS CoreFoundation + ImageIO FFI for oriented JPEG/PNG/HEIC decoding and JPEG encoding.

#![cfg(target_os = "macos")]

use std::ffi::{c_char, c_void};
use std::ptr;

use crate::media::MediaError;
use crate::types::PhotoStatus;

type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFDataRef = *const c_void;
type CFMutableDataRef = *mut c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
type CFNumberRef = *const c_void;
type CFBooleanRef = *const c_void;
type CGImageSourceRef = *const c_void;
type CGImageDestinationRef = *mut c_void;
type CGImageRef = *const c_void;
type CFIndex = isize;
type CFStringEncoding = u32;
type CFNumberType = isize;

const K_CF_STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;
const K_CF_NUMBER_SINT64_TYPE: CFNumberType = 4;
const K_CF_NUMBER_FLOAT64_TYPE: CFNumberType = 6;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFAllocatorDefault: CFAllocatorRef;
    static kCFBooleanTrue: CFBooleanRef;
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;

    fn CFRelease(cf: CFTypeRef);
    fn CFDataCreate(allocator: CFAllocatorRef, bytes: *const u8, length: CFIndex) -> CFDataRef;
    fn CFDataCreateMutable(allocator: CFAllocatorRef, capacity: CFIndex) -> CFMutableDataRef;
    fn CFDataGetLength(the_data: CFDataRef) -> CFIndex;
    fn CFDataGetBytePtr(the_data: CFDataRef) -> *const u8;
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const c_char,
        encoding: CFStringEncoding,
    ) -> CFStringRef;
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: CFNumberType,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    fn CFNumberGetValue(
        number: CFNumberRef,
        the_type: CFNumberType,
        value_ptr: *mut c_void,
    ) -> bool;
    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: CFIndex,
        key_callbacks: *const u8,
        value_callbacks: *const u8,
    ) -> CFDictionaryRef;
    fn CFDictionaryGetValue(the_dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
}

#[link(name = "ImageIO", kind = "framework")]
extern "C" {
    static kCGImageSourceCreateThumbnailFromImageAlways: CFStringRef;
    static kCGImageSourceCreateThumbnailWithTransform: CFStringRef;
    static kCGImageSourceThumbnailMaxPixelSize: CFStringRef;
    static kCGImageSourceShouldCacheImmediately: CFStringRef;
    static kCGImagePropertyPixelWidth: CFStringRef;
    static kCGImagePropertyPixelHeight: CFStringRef;
    static kCGImageDestinationLossyCompressionQuality: CFStringRef;

    fn CGImageSourceCreateWithData(data: CFDataRef, options: CFDictionaryRef) -> CGImageSourceRef;
    fn CGImageSourceGetCount(isrc: CGImageSourceRef) -> usize;
    fn CGImageSourceCopyPropertiesAtIndex(
        isrc: CGImageSourceRef,
        index: usize,
        options: CFDictionaryRef,
    ) -> CFDictionaryRef;
    fn CGImageSourceCreateImageAtIndex(
        isrc: CGImageSourceRef,
        index: usize,
        options: CFDictionaryRef,
    ) -> CGImageRef;
    fn CGImageSourceCreateThumbnailAtIndex(
        isrc: CGImageSourceRef,
        index: usize,
        options: CFDictionaryRef,
    ) -> CGImageRef;

    fn CGImageDestinationCreateWithData(
        data: CFMutableDataRef,
        type_: CFStringRef,
        count: usize,
        options: CFDictionaryRef,
    ) -> CGImageDestinationRef;
    fn CGImageDestinationAddImage(
        idst: CGImageDestinationRef,
        image: CGImageRef,
        properties: CFDictionaryRef,
    );
    fn CGImageDestinationFinalize(idst: CGImageDestinationRef) -> bool;
}

struct OwnedCf(CFTypeRef);

impl OwnedCf {
    fn from_ptr(ptr: CFTypeRef) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self(ptr))
        }
    }

    fn as_ptr(&self) -> CFTypeRef {
        self.0
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // Safety: `OwnedCf` exclusively owns this CoreFoundation object reference.
            unsafe { CFRelease(self.0) };
        }
    }
}

fn dict_get_i64(dict: CFDictionaryRef, key: CFStringRef) -> Option<i64> {
    // Safety: `dict` and `key` are valid CF references from ImageIO.
    let val = unsafe { CFDictionaryGetValue(dict, key) };
    if val.is_null() {
        return None;
    }
    let mut out: i64 = 0;
    let ok = unsafe {
        CFNumberGetValue(
            val,
            K_CF_NUMBER_SINT64_TYPE,
            (&raw mut out).cast::<c_void>(),
        )
    };
    if ok && out > 0 {
        Some(out)
    } else {
        None
    }
}

/// Decode bytes via Apple `ImageIO` (oriented primary still), resize long edge to at most
/// `long_edge` without upscaling, and encode as JPEG at quality 0.85.
pub fn render_jpeg_from_bytes(
    bytes: &[u8],
    long_edge: u32,
    quality: f64,
) -> Result<Vec<u8>, MediaError> {
    if bytes.is_empty() {
        return Err(MediaError::new(
            "unreadable: empty image file",
            PhotoStatus::SkippedUnreadable,
        ));
    }

    // Safety: FFI calls validate null returns and all owned CF objects are released via RAII.
    unsafe {
        let cf_data = OwnedCf::from_ptr(CFDataCreate(
            kCFAllocatorDefault,
            bytes.as_ptr(),
            bytes.len() as CFIndex,
        ))
        .ok_or_else(|| MediaError::new("CFDataCreate failed", PhotoStatus::Error))?;

        let src = OwnedCf::from_ptr(CGImageSourceCreateWithData(cf_data.as_ptr(), ptr::null()))
            .ok_or_else(|| {
                MediaError::new(
                    "unreadable: CGImageSourceCreateWithData returned null",
                    PhotoStatus::SkippedUnreadable,
                )
            })?;

        if CGImageSourceGetCount(src.as_ptr()) == 0 {
            return Err(MediaError::new(
                "unreadable: image source contains no frames",
                PhotoStatus::SkippedUnreadable,
            ));
        }

        let props = OwnedCf::from_ptr(CGImageSourceCopyPropertiesAtIndex(
            src.as_ptr(),
            0,
            ptr::null(),
        ));
        let target_max = match &props {
            Some(p) => {
                let w = dict_get_i64(p.as_ptr(), kCGImagePropertyPixelWidth).unwrap_or(0);
                let h = dict_get_i64(p.as_ptr(), kCGImagePropertyPixelHeight).unwrap_or(0);
                let max_dim = w.max(h);
                if max_dim > 0 {
                    (long_edge as i64).min(max_dim)
                } else {
                    long_edge as i64
                }
            }
            None => long_edge as i64,
        };

        let max_num = OwnedCf::from_ptr(CFNumberCreate(
            kCFAllocatorDefault,
            K_CF_NUMBER_SINT64_TYPE,
            (&raw const target_max).cast::<c_void>(),
        ))
        .ok_or_else(|| MediaError::new("CFNumberCreate failed", PhotoStatus::Error))?;

        let keys = [
            kCGImageSourceCreateThumbnailFromImageAlways,
            kCGImageSourceCreateThumbnailWithTransform,
            kCGImageSourceThumbnailMaxPixelSize,
            kCGImageSourceShouldCacheImmediately,
        ];
        let vals = [
            kCFBooleanTrue,
            kCFBooleanTrue,
            max_num.as_ptr(),
            kCFBooleanTrue,
        ];
        let opts = OwnedCf::from_ptr(CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr().cast(),
            vals.as_ptr().cast(),
            keys.len() as CFIndex,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        ))
        .ok_or_else(|| MediaError::new("CFDictionaryCreate failed", PhotoStatus::Error))?;

        let cg_image = OwnedCf::from_ptr(CGImageSourceCreateThumbnailAtIndex(
            src.as_ptr(),
            0,
            opts.as_ptr(),
        ))
        .ok_or_else(|| {
            MediaError::new(
                "unreadable: CGImageSourceCreateThumbnailAtIndex returned null",
                PhotoStatus::SkippedUnreadable,
            )
        })?;

        let out_data_ptr = CFDataCreateMutable(kCFAllocatorDefault, 0);
        let out_data = OwnedCf::from_ptr(out_data_ptr.cast())
            .ok_or_else(|| MediaError::new("CFDataCreateMutable failed", PhotoStatus::Error))?;

        let jpeg_uti = OwnedCf::from_ptr(CFStringCreateWithCString(
            kCFAllocatorDefault,
            b"public.jpeg\0".as_ptr().cast::<c_char>(),
            K_CF_STRING_ENCODING_UTF8,
        ))
        .ok_or_else(|| MediaError::new("CFStringCreateWithCString failed", PhotoStatus::Error))?;

        let dest_ptr =
            CGImageDestinationCreateWithData(out_data_ptr, jpeg_uti.as_ptr(), 1, ptr::null());
        let _dest = OwnedCf::from_ptr(dest_ptr.cast())
            .ok_or_else(|| MediaError::new("CGImageDestinationCreateWithData failed", PhotoStatus::Error))?;

        let q_num = OwnedCf::from_ptr(CFNumberCreate(
            kCFAllocatorDefault,
            K_CF_NUMBER_FLOAT64_TYPE,
            (&raw const quality).cast::<c_void>(),
        ))
        .ok_or_else(|| MediaError::new("CFNumberCreate failed", PhotoStatus::Error))?;

        let enc_keys = [kCGImageDestinationLossyCompressionQuality];
        let enc_vals = [q_num.as_ptr()];
        let enc_opts = OwnedCf::from_ptr(CFDictionaryCreate(
            kCFAllocatorDefault,
            enc_keys.as_ptr().cast(),
            enc_vals.as_ptr().cast(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        ))
        .ok_or_else(|| MediaError::new("CFDictionaryCreate failed", PhotoStatus::Error))?;

        CGImageDestinationAddImage(dest_ptr, cg_image.as_ptr(), enc_opts.as_ptr());
        if !CGImageDestinationFinalize(dest_ptr) {
            return Err(MediaError::new(
                "unreadable: CGImageDestinationFinalize failed",
                PhotoStatus::SkippedUnreadable,
            ));
        }

        let len = CFDataGetLength(out_data.as_ptr());
        if len <= 0 {
            return Err(MediaError::new(
                "unreadable: encoded JPEG is empty",
                PhotoStatus::SkippedUnreadable,
            ));
        }
        let ptr = CFDataGetBytePtr(out_data.as_ptr());
        let slice = std::slice::from_raw_parts(ptr, len as usize);
        Ok(slice.to_vec())
    }
}

/// Encode source image bytes to HEIC bytes in memory via Apple `ImageIO` (used by tests).
pub fn encode_heic_from_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let cf_data = OwnedCf::from_ptr(CFDataCreate(
            kCFAllocatorDefault,
            bytes.as_ptr(),
            bytes.len() as CFIndex,
        ))?;
        let src = OwnedCf::from_ptr(CGImageSourceCreateWithData(cf_data.as_ptr(), ptr::null()))?;
        if CGImageSourceGetCount(src.as_ptr()) == 0 {
            return None;
        }
        let cg_image = OwnedCf::from_ptr(CGImageSourceCreateImageAtIndex(
            src.as_ptr(),
            0,
            ptr::null(),
        ))?;
        let out_data_ptr = CFDataCreateMutable(kCFAllocatorDefault, 0);
        let out_data = OwnedCf::from_ptr(out_data_ptr.cast())?;
        let heic_uti = OwnedCf::from_ptr(CFStringCreateWithCString(
            kCFAllocatorDefault,
            b"public.heic\0".as_ptr().cast::<c_char>(),
            K_CF_STRING_ENCODING_UTF8,
        ))?;
        let dest_ptr =
            CGImageDestinationCreateWithData(out_data_ptr, heic_uti.as_ptr(), 1, ptr::null());
        let _dest = OwnedCf::from_ptr(dest_ptr.cast())?;
        CGImageDestinationAddImage(dest_ptr, cg_image.as_ptr(), ptr::null());
        if !CGImageDestinationFinalize(dest_ptr) {
            return None;
        }
        let len = CFDataGetLength(out_data.as_ptr());
        if len <= 0 {
            return None;
        }
        let ptr = CFDataGetBytePtr(out_data.as_ptr());
        Some(std::slice::from_raw_parts(ptr, len as usize).to_vec())
    }
}
