//! macOS: talking to a module's USB device beside the HID driver, through IOUSBHost (macOS 10.15+). The device
//! object is opened without capture, which needs neither root nor an entitlement. Plain IOKit and Objective-C
//! runtime calls, no extra crate.
//! - `set_idle`: the 2xRoller only starts sending its input once it got the standard HID class request
//!   SET_IDLE, which Linux and Windows send to every HID device and macOS only to keyboards and mice — so on a
//!   Mac the rollers stayed silent (found on the user's unit, 2026-10-25).
//! - `reenumerate`: a stuck 15xKey or dial comes back clean, as if taken off the base and put back
//!   (IOUSBHostDevice `resetWithError:`).

use std::ffi::{c_char, c_void, CStr};

/// The IORegistry id in a macOS hidapi path ("DevSrvsID:4294969374").
pub fn registry_id(hid_path: &str) -> Option<u64> {
    hid_path.strip_prefix("DevSrvsID:")?.trim().parse().ok()
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IORegistryEntryIDMatching(entry_id: u64) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IOObjectConformsTo(object: u32, class_name: *const c_char) -> u32;
    fn IORegistryEntryGetParentEntry(entry: u32, plane: *const c_char, parent: *mut u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
}

const USB_DEVICE: &CStr = c"IOUSBHostDevice";

/// IOUSBDeviceRequest: a USB control request.
#[repr(C)]
#[derive(Clone, Copy)]
struct Request {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
}

type Id = *mut c_void;

fn sel(name: &CStr) -> Id {
    unsafe { sel_registerName(name.as_ptr()) }
}

/// Opens the USB device above the HID service with this registry id, runs `f` on the IOUSBHostDevice object and
/// closes it again.
fn with_device<T>(registry_id: u64, f: impl FnOnce(Id) -> Result<T, &'static str>) -> Result<T, &'static str> {
    // SAFETY: IOKit objects are released on every path; Objective-C messages are sent with the exact
    // signatures of the IOUSBHostDevice methods.
    unsafe {
        let mut entry = IOServiceGetMatchingService(0, IORegistryEntryIDMatching(registry_id)); // 0: default port
        if entry == 0 {
            return Err("no such HID service");
        }
        // walk up the service plane to the USB device that holds this HID interface
        for _ in 0..8 {
            if IOObjectConformsTo(entry, USB_DEVICE.as_ptr()) != 0 {
                break;
            }
            let mut parent = 0;
            let kr = IORegistryEntryGetParentEntry(entry, c"IOService".as_ptr(), &mut parent);
            IOObjectRelease(entry);
            entry = if kr == 0 { parent } else { 0 };
            if entry == 0 {
                return Err("no USB device above the HID service");
            }
        }
        if IOObjectConformsTo(entry, USB_DEVICE.as_ptr()) == 0 {
            IOObjectRelease(entry);
            return Err("no USB device above the HID service");
        }
        dlopen(c"/System/Library/Frameworks/IOUSBHost.framework/IOUSBHost".as_ptr(), 2); // RTLD_NOW
        let class = objc_getClass(USB_DEVICE.as_ptr());
        if class.is_null() {
            IOObjectRelease(entry);
            return Err("IOUSBHost isn't available");
        }
        let pool = objc_autoreleasePoolPush();
        let send = objc_msgSend as unsafe extern "C" fn();
        let alloc: unsafe extern "C" fn(Id, Id) -> Id = std::mem::transmute(send);
        let init: unsafe extern "C" fn(Id, Id, u32, u64, Id, *mut Id, Id) -> Id = std::mem::transmute(send);
        let plain: unsafe extern "C" fn(Id, Id) = std::mem::transmute(send);
        let mut error = std::ptr::null_mut();
        let device = alloc(class, sel(c"alloc"));
        let device = init(
            device,
            sel(c"initWithIOService:options:queue:error:interestHandler:"),
            entry,
            0,
            std::ptr::null_mut(),
            &mut error,
            std::ptr::null_mut(),
        );
        IOObjectRelease(entry);
        let result = if device.is_null() {
            Err("the USB device can't be opened (busy)")
        } else {
            let r = f(device);
            plain(device, sel(c"destroy"));
            plain(device, sel(c"release"));
            r
        };
        objc_autoreleasePoolPop(pool);
        result
    }
}

/// Sends SET_IDLE(0) (report on change only, all report ids) to interface 0 of the module's USB device.
pub fn set_idle(registry_id: u64) -> Result<(), &'static str> {
    with_device(registry_id, |device| unsafe {
        let send = objc_msgSend as unsafe extern "C" fn();
        let request: unsafe extern "C" fn(Id, Id, Request, Id, *mut u64, f64, *mut Id) -> i8 =
            std::mem::transmute(send);
        let set_idle = Request { request_type: 0x21, request: 0x0a, value: 0, index: 0, length: 0 };
        let (mut done, mut error) = (0u64, std::ptr::null_mut());
        let ok = request(
            device,
            sel(c"sendDeviceRequest:data:bytesTransferred:completionTimeout:error:"),
            set_idle,
            std::ptr::null_mut(),
            &mut done,
            2.0,
            &mut error,
        );
        if ok != 0 {
            Ok(())
        } else {
            Err("SET_IDLE was rejected")
        }
    })
}

/// Re-enumerates the USB device above the HID service with this registry id.
pub fn reenumerate(registry_id: u64) -> Result<(), &'static str> {
    with_device(registry_id, |device| unsafe {
        let send = objc_msgSend as unsafe extern "C" fn();
        let reset: unsafe extern "C" fn(Id, Id, *mut Id) -> i8 = std::mem::transmute(send);
        let mut error = std::ptr::null_mut();
        if reset(device, sel(c"resetWithError:"), &mut error) != 0 {
            Ok(())
        } else {
            Err("the reset failed")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_registry_id() {
        assert_eq!(registry_id("DevSrvsID:4294969374"), Some(4294969374));
        assert_eq!(registry_id("IOService:/AppleARMPE/…"), None);
    }

    #[test]
    fn unknown_service_fails_cleanly() {
        // exercises the IOKit calls without touching any device
        assert_eq!(reenumerate(1), Err("no such HID service"));
        assert_eq!(set_idle(1), Err("no such HID service"));
    }
}
