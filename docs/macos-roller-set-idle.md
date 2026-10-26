# 2xRoller on macOS: send SET_IDLE and it works

**Summary.** The MasterHUB 2xRoller (M032LE, `2516:0204`) sends no input reports on macOS because its firmware
only starts reporting after the standard HID class request **SET_IDLE**. Linux and Windows send SET_IDLE to every
HID device when they bind it; macOS sends it only to keyboards and mice. Sending one SET_IDLE(0) to the roller —
from user space, no root, no entitlement, without taking the device from the HID driver — makes it report
normally (`01 2a 04 00 01 02 R L`). Found and fixed in this plugin on 2026-10-25
(current macOS on Apple Silicon, roller firmware 01.00.03.0x).

## Symptoms

- The roller enumerates fine (`2xRoller, 4x1`, usage page 0x0C), opens through hidapi / IOHIDManager, and its
  report descriptor is identical in structure to the knob's (input report 0x01, 63 bytes).
- Its **control pipe works**: feature reports (0x2D–0x31, 0x36) answer, `GET_REPORT` for input report 0x01 answers
  (all zeros). Nothing changes while turning.
- Its **interrupt IN endpoint stays silent**: no reports through hidapi or IOHIDManager, no transfer errors in the
  kernel log. The knob on the same base reports normally at the same time.
- macOS's HID driver logs identical pipe parameters for roller and knob
  (`inputReportSize:64 inMaxPacketSize:64`), so it isn't a size mismatch.

## Proof

One SET_IDLE(0) to interface 0, then reading the HID device as before: **731 roller reports in 10 s** of turning,
in the format already documented (byte 6 right roller, byte 7 left; `00` up, `01` down, `ff` none):

```
SET_IDLE(0): Ok(())
64 bytes [01, 2a, 04, 00, 01, 02, ff, 00, ...]
64 bytes [01, 2a, 04, 00, 01, 02, 01, ff, ...]
TOTAL interrupt reports: 731
```

It lasts until the module is reset (replug, base power loss, sleep), so send it **every time the roller
connects**.

## The request

SET_IDLE, interface recipient, duration 0 (report only on change), all report ids:

| bmRequestType | bRequest | wValue | wIndex | wLength |
| --- | --- | --- | --- | --- |
| `0x21` | `0x0A` (SET_IDLE) | `0x0000` | `0x0000` (interface 0) | `0` |

## Sending it on macOS

IOUSBHost (macOS 10.15+) can open the `IOUSBHostDevice` **without capture** (options `0`) while the HID driver
keeps the interface, and send a control request on the default pipe. Walk up from the HID service (hidapi's
macOS path is `DevSrvsID:<registry id>`) to the `IOUSBHostDevice`.

### Objective-C

```objc
#import <IOKit/IOKitLib.h>
#import <IOUSBHost/IOUSBHost.h>

// registryID: from the hidapi path "DevSrvsID:<id>". Returns YES when the roller accepted SET_IDLE.
BOOL roller_set_idle(uint64_t registryID) {
    @autoreleasepool {
        io_service_t entry = IOServiceGetMatchingService(kIOMainPortDefault, IORegistryEntryIDMatching(registryID));
        for (int i = 0; i < 8 && entry && !IOObjectConformsTo(entry, "IOUSBHostDevice"); i++) {
            io_registry_entry_t parent = IO_OBJECT_NULL;
            kern_return_t kr = IORegistryEntryGetParentEntry(entry, kIOServicePlane, &parent);
            IOObjectRelease(entry);
            entry = kr == KERN_SUCCESS ? parent : IO_OBJECT_NULL;
        }
        if (!entry) return NO;
        NSError *error = nil;
        IOUSBHostDevice *device = [[IOUSBHostDevice alloc] initWithIOService:entry
                                                                     options:IOUSBHostObjectInitOptionsNone
                                                                       queue:nil error:&error interestHandler:nil];
        IOObjectRelease(entry);
        if (!device) return NO;
        IOUSBDeviceRequest setIdle = { .bmRequestType = 0x21, .bRequest = 0x0A, .wValue = 0, .wIndex = 0, .wLength = 0 };
        NSUInteger done = 0;
        BOOL ok = [device sendDeviceRequest:setIdle data:nil bytesTransferred:&done completionTimeout:2.0 error:&error];
        [device destroy];
        return ok;
    }
}
```

### Rust (FFI, no extra crate)

The plugin's implementation is `set_idle(registry_id)` in `src/usbhost.rs`: it opens the device as above through
`objc_msgSend` and sends the request with
`sendDeviceRequest:data:bytesTransferred:completionTimeout:error:`. The request struct is passed by value:

```rust
#[repr(C)]
#[derive(Clone, Copy)]
struct Request { request_type: u8, request: u8, value: u16, index: u16, length: u16 }

let set_idle = Request { request_type: 0x21, request: 0x0a, value: 0, index: 0, length: 0 };
// fn(device, selector, Request, NSMutableData* (null), *mut NSUInteger, NSTimeInterval, *mut NSError*) -> BOOL
```

Mind the selector: `sendDeviceRequest:…` — `deviceRequest:…` doesn't exist and throws an Objective-C exception.

## Notes

- Only the roller needs it; keypad, knobs, faders, dial and base report without it. Sending it to them is harmless.
- The roller doesn't need software mode or any vendor command first.
- Linux (hidraw / usbhid) and Windows (hidclass) already send SET_IDLE, which is why the roller works there.
- A side observation from the same session: modules dropping off on macOS were logged by the kernel as
  `hardware connection lost` (contact or power), unrelated to this.
