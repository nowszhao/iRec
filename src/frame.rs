//! Frame copy + crop.
//!
//! Two reasons we never hand ScreenCaptureKit frames straight to AVAssetWriter:
//!
//! 1. Asking ScreenCaptureKit for a sub-region (`sourceRect`) produces frames
//!    that VideoToolbox refuses to encode (`appendSampleBuffer:` fails with
//!    AVFoundationErrorDomain -11800 / NSOSStatusErrorDomain -16122), even when
//!    the frame is copied into a buffer we own. So we always capture the whole
//!    display and crop here instead.
//! 2. Copying into buffers from our own pool also decouples us from
//!    ScreenCaptureKit's recycled IOSurfaces.

use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_av_foundation::{AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
use objc2_core_media::CMTime;
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferPoolMinimumBufferCountKey, kCVPixelBufferWidthKey, CVPixelBuffer,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPlaneCount,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress, CVPixelBufferGetWidth,
};
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString};

/// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
const PIXEL_FORMAT_420V: u32 = 0x34323076;
const LOCK_READ_ONLY: CVPixelBufferLockFlags = CVPixelBufferLockFlags(1);
const LOCK_NONE: CVPixelBufferLockFlags = CVPixelBufferLockFlags(0);

type Attrs = NSMutableDictionary<NSString, AnyObject>;

/// Rectangle to copy out of each captured frame, in captured pixels.
#[derive(Clone, Copy, Debug)]
pub struct Crop {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

pub struct FrameWriter {
    pool: CFRetained<CVPixelBufferPool>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    crop: Crop,
}

impl FrameWriter {
    pub fn new(input: &AVAssetWriterInput, crop: Crop) -> Result<Self, String> {
        Self::with_size(input, crop.width, crop.height, crop)
    }

    fn with_size(
        input: &AVAssetWriterInput,
        width: usize,
        height: usize,
        crop: Crop,
    ) -> Result<Self, String> {
        let attrs: Retained<Attrs> = NSMutableDictionary::new();
        unsafe {
            set_number(&attrs, cf_key(kCVPixelBufferPixelFormatTypeKey), PIXEL_FORMAT_420V as f64);
            set_number(&attrs, cf_key(kCVPixelBufferWidthKey), width as f64);
            set_number(&attrs, cf_key(kCVPixelBufferHeightKey), height as f64);
            // Ask for IOSurface backing so the hardware encoder can stay on the GPU.
            let iosurface: Retained<Attrs> = NSMutableDictionary::new();
            let iosurface_obj: &AnyObject = &iosurface;
            attrs.setObject_forKey(
                iosurface_obj,
                ProtocolObject::from_ref(cf_key(kCVPixelBufferIOSurfacePropertiesKey)),
            );
        }

        let pool_attrs: Retained<Attrs> = NSMutableDictionary::new();
        unsafe {
            set_number(
                &pool_attrs,
                cf_key(kCVPixelBufferPoolMinimumBufferCountKey),
                4.0,
            );
        }

        let cf_attrs: &CFDictionary =
            unsafe { &*(&*attrs as *const Attrs as *const CFDictionary) };
        let cf_pool_attrs: &CFDictionary =
            unsafe { &*(&*pool_attrs as *const Attrs as *const CFDictionary) };

        let mut pool_out: *mut CVPixelBufferPool = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create(
                None,
                Some(cf_pool_attrs),
                Some(cf_attrs),
                NonNull::new_unchecked(&mut pool_out as *mut *mut CVPixelBufferPool),
            )
        };
        if status != 0 {
            return Err(format!("CVPixelBufferPool 创建失败（{status}）"));
        }
        let pool = unsafe { CFRetained::from_raw(NonNull::new(pool_out).unwrap()) };

        let adaptor = unsafe {
            AVAssetWriterInputPixelBufferAdaptor::initWithAssetWriterInput_sourcePixelBufferAttributes(
                AVAssetWriterInputPixelBufferAdaptor::alloc(),
                input,
                Some(&attrs),
            )
        };

        Ok(Self { pool, adaptor, crop })
    }

    /// Copies `src` into a buffer we own and appends it at `pts`.
    pub fn append(&self, src: &CVPixelBuffer, pts: CMTime) -> Result<bool, String> {
        let mut dst_ptr: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer(
                None,
                &self.pool,
                NonNull::new_unchecked(&mut dst_ptr as *mut *mut CVPixelBuffer),
            )
        };
        if status != 0 || dst_ptr.is_null() {
            return Err(format!("像素缓冲区分配失败（{status}）"));
        }
        let dst = unsafe { CFRetained::from_raw(NonNull::new(dst_ptr).unwrap()) };

        if std::env::var("SCREEN_REC_DEBUG").is_ok() {
            eprintln!(
                "irec: crop {:?} from {}x{} -> {}x{}",
                self.crop,
                CVPixelBufferGetWidth(src),
                CVPixelBufferGetHeight(src),
                CVPixelBufferGetWidth(&dst),
                CVPixelBufferGetHeight(&dst),
            );
        }
        unsafe {
            CVPixelBufferLockBaseAddress(src, LOCK_READ_ONLY);
            CVPixelBufferLockBaseAddress(&dst, LOCK_NONE);

            // NV12: plane 0 is luma (full size), plane 1 is interleaved chroma
            // with half the height and the same byte width.
            let planes = CVPixelBufferGetPlaneCount(src).min(2);
            for plane in 0..planes {
                let src_base = CVPixelBufferGetBaseAddressOfPlane(src, plane);
                let dst_base = CVPixelBufferGetBaseAddressOfPlane(&dst, plane);
                if src_base.is_null() || dst_base.is_null() {
                    continue;
                }
                let src_stride = CVPixelBufferGetBytesPerRowOfPlane(src, plane);
                let dst_stride = CVPixelBufferGetBytesPerRowOfPlane(&dst, plane);
                if src_stride == 0 || dst_stride == 0 {
                    continue;
                }
                let vertical_scale = if plane == 0 { 1 } else { 2 };
                let start_row = self.crop.y / vertical_scale;
                let rows = (self.crop.height / vertical_scale)
                    .min(CVPixelBufferGetHeightOfPlane(&dst, plane))
                    .min(CVPixelBufferGetHeightOfPlane(src, plane).saturating_sub(start_row));
                let bytes = self
                    .crop
                    .width
                    .min(src_stride.saturating_sub(self.crop.x))
                    .min(dst_stride);
                let src_base = (src_base as *const u8).add(start_row * src_stride + self.crop.x);
                let dst_base = dst_base as *mut u8;
                for row in 0..rows {
                    std::ptr::copy_nonoverlapping(
                        src_base.add(row * src_stride),
                        dst_base.add(row * dst_stride),
                        bytes,
                    );
                }
            }

            CVPixelBufferUnlockBaseAddress(&dst, LOCK_NONE);
            CVPixelBufferUnlockBaseAddress(src, LOCK_READ_ONLY);

            Ok(self.adaptor.appendPixelBuffer_withPresentationTime(&dst, pts))
        }
    }
}

/// CFString keys are toll-free bridged with NSString.
unsafe fn cf_key(key: &'static CFString) -> &'static NSString { unsafe {
    &*(key as *const CFString as *const NSString)
}}

unsafe fn set_number(dict: &Attrs, key: &'static NSString, value: f64) { unsafe {
    let number = NSNumber::numberWithDouble(value);
    let object: &AnyObject = &*number;
    dict.setObject_forKey(object, ProtocolObject::from_ref(key));
}}
