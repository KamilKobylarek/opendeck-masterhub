//! Where a module sits on the base, from its USB port chain.
//!
//! The official app (docs/masterhub-app-analysis.md) finds the base's Prolific hub and takes the
//! module's ports below it, joined without separators ("143"). Every contact field leads to its
//! own port, so that key gives both the position and the rotation of the module.

#[cfg(target_os = "macos")]
use std::process::Command;

const HUB_VID: u16 = 0x067b;
const HUB_PID: u16 = 0x2586;
const MODULE_VID: u16 = 0x2516;
const BASE_PIDS: [u16; 2] = [0x01f6, 0x9999]; // M0064

#[derive(Debug, PartialEq)]
pub struct UsbLoc {
    pub vid: u16,
    pub pid: u16,
    pub serial: String,
    /// Port chain from the host controller down; the first element is the bus / root hub.
    pub chain: Vec<u32>,
}

/// Port key of a module, e.g. "22" or "143", or None if it isn't below a MasterHUB hub.
pub fn port_key(locs: &[UsbLoc], pids: &[u16], serial: &str) -> Option<String> {
    let mine = |l: &&UsbLoc| l.vid == MODULE_VID && pids.contains(&l.pid);
    let module = locs.iter().filter(mine).find(|l| l.serial.eq_ignore_ascii_case(serial)).or_else(|| {
        // No usable serial: still fine when only one module of this type is connected.
        let mut same = locs.iter().filter(mine);
        let first = same.next()?;
        same.next().is_none().then_some(first)
    })?;
    let base = locs
        .iter()
        .find(|l| l.vid == MODULE_VID && BASE_PIDS.contains(&l.pid) && l.chain.starts_with(&module.chain[..1]));
    // The base hub is the outermost Prolific hub above the module (and, like the app checks,
    // at least two levels above the M0064 controller when that one is visible).
    let hub = locs
        .iter()
        .filter(|l| {
            l.vid == HUB_VID
                && l.pid == HUB_PID
                && module.chain.len() > l.chain.len()
                && module.chain.starts_with(&l.chain)
        })
        .filter(|l| base.is_none_or(|b| b.chain.len() >= l.chain.len() + 2 && b.chain.starts_with(&l.chain)))
        .min_by_key(|l| l.chain.len())?;
    Some(module.chain[hub.chain.len()..].iter().map(u32::to_string).collect())
}

#[cfg(target_os = "macos")]
pub fn usb_locations() -> Vec<UsbLoc> {
    // hidapi only exposes an IOService id on macOS, so ask ioreg for the USB locationID.
    match crate::command::tool_output(
        Command::new("ioreg").args(["-p", "IOUSB", "-l", "-w0"]),
        std::time::Duration::from_secs(10),
    ) {
        Some(out) => parse_ioreg(&out),
        None => {
            eprintln!("ioreg failed");
            Vec::new()
        }
    }
}

#[cfg(windows)]
pub fn usb_locations() -> Vec<UsbLoc> {
    parse_pnp(&pnp_lines())
}

/// "InstanceId|LocationPath;LocationPath" per present USB device of ours, read from the device tree (CfgMgr32):
/// what PowerShell's Get-PnpDevice gives, in milliseconds and without starting it (it took over 20 s on some PCs).
#[cfg(windows)]
fn pnp_lines() -> String {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_PropertyW, CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW, CM_Locate_DevNodeW,
        CM_GETIDLIST_FILTER_ENUMERATOR, CM_GETIDLIST_FILTER_PRESENT, CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS,
    };
    use windows::Win32::Devices::Properties::{DEVPKEY_Device_LocationPaths, DEVPROPTYPE};
    // a multi-string: strings ended by NULs, the list by an empty one
    let strings = |wide: &[u16]| -> Vec<String> {
        wide.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    };
    let flags = CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT;
    let mut out = String::new();
    unsafe {
        let mut len = 0u32;
        if CM_Get_Device_ID_List_SizeW(&mut len, w!("USB"), flags) != CR_SUCCESS {
            eprintln!("cannot list USB devices");
            return out;
        }
        let mut ids = vec![0u16; len as usize];
        if CM_Get_Device_ID_ListW(w!("USB"), &mut ids, flags) != CR_SUCCESS {
            eprintln!("cannot list USB devices");
            return out;
        }
        for id in strings(&ids) {
            let upper = id.to_ascii_uppercase();
            if !upper.contains("VID_2516") && !upper.contains("VID_067B&PID_2586") {
                continue;
            }
            let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
            let mut node = 0u32;
            if CM_Locate_DevNodeW(&mut node, PCWSTR(wide.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS {
                continue;
            }
            let (mut kind, mut size) = (DEVPROPTYPE::default(), 0u32);
            let _ = CM_Get_DevNode_PropertyW(node, &DEVPKEY_Device_LocationPaths, &mut kind, None, &mut size, 0);
            let mut buf = vec![0u8; size as usize];
            if size == 0
                || CM_Get_DevNode_PropertyW(
                    node,
                    &DEVPKEY_Device_LocationPaths,
                    &mut kind,
                    Some(buf.as_mut_ptr()),
                    &mut size,
                    0,
                ) != CR_SUCCESS
            {
                continue;
            }
            let wide: Vec<u16> = buf.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
            out += &format!("{id}|{}\n", strings(&wide).join(";"));
        }
    }
    out
}

#[cfg(target_os = "linux")]
pub fn usb_locations() -> Vec<UsbLoc> {
    // Every USB device has a sysfs entry named after its port chain, with its ids next to it.
    let Ok(dir) = std::fs::read_dir("/sys/bus/usb/devices") else {
        eprintln!("cannot read /sys/bus/usb/devices");
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|entry| {
            let chain = sysfs_chain(entry.file_name().to_str()?)?;
            let read = |f: &str| std::fs::read_to_string(entry.path().join(f)).ok().map(|s| s.trim().to_string());
            let vid = u16::from_str_radix(&read("idVendor")?, 16).ok()?;
            let pid = u16::from_str_radix(&read("idProduct")?, 16).ok()?;
            relevant(vid, pid).then(|| UsbLoc { vid, pid, serial: read("serial").unwrap_or_default(), chain })
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub fn usb_locations() -> Vec<UsbLoc> {
    Vec::new()
}

/// sysfs device name -> port chain: "1-2.1.4" is bus 1, ports 2, 1, 4. Root hubs ("usb1") and
/// interfaces ("1-2.1.4:1.0") are not devices of their own.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn sysfs_chain(name: &str) -> Option<Vec<u32>> {
    let (bus, ports) = name.split_once('-')?;
    if ports.contains(':') {
        return None;
    }
    std::iter::once(bus).chain(ports.split('.')).map(|p| p.parse().ok()).collect()
}

fn relevant(vid: u16, pid: u16) -> bool {
    vid == MODULE_VID || (vid == HUB_VID && pid == HUB_PID)
}

/// Parses `ioreg -p IOUSB -l -w0`: one "+-o" header per USB device, followed by its properties.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse_ioreg(text: &str) -> Vec<UsbLoc> {
    fn flush(props: &[(&str, &str)], out: &mut Vec<UsbLoc>) {
        let get = |key: &str| props.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        let num = |key: &str| get(key).and_then(|v| v.parse::<u32>().ok());
        let (Some(vid), Some(pid), Some(loc)) = (num("idVendor"), num("idProduct"), num("locationID")) else { return };
        let (vid, pid) = (vid as u16, pid as u16);
        if relevant(vid, pid) {
            let serial = get("USB Serial Number").unwrap_or("").trim_matches('"').to_string();
            out.push(UsbLoc { vid, pid, serial, chain: mac_chain(loc) });
        }
    }

    let mut out = Vec::new();
    let mut props: Vec<(&str, &str)> = Vec::new();
    for line in text.lines() {
        if line.contains("+-o ") {
            flush(&props, &mut out);
            props.clear();
        } else if let Some((k, v)) = line.split_once(" = ") {
            props.push((k.trim_start_matches([' ', '|']).trim().trim_matches('"'), v.trim()));
        }
    }
    flush(&props, &mut out);
    out
}

/// macOS locationID: top byte is the bus, then one nibble per port until a zero nibble.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_chain(loc: u32) -> Vec<u32> {
    let mut chain = vec![loc >> 24];
    for shift in (0..6).rev() {
        let port = (loc >> (shift * 4)) & 0xf;
        if port == 0 {
            break;
        }
        chain.push(port);
    }
    chain
}

/// Parses "InstanceId|LocationPaths" lines, e.g.
/// `USB\VID_2516&PID_01FE\SN1|PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(3)`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse_pnp(text: &str) -> Vec<UsbLoc> {
    let hex = |s: &str, key: &str| u16::from_str_radix(s.split(key).nth(1)?.get(..4)?, 16).ok();
    text.lines()
        .filter_map(|line| {
            let (instance, paths) = line.trim().split_once('|')?;
            let mut parts = instance.split('\\');
            let ids = parts.nth(1)?.to_ascii_uppercase();
            if ids.contains("&MI_") {
                return None; // an interface of a composite device; its parent carries the location
            }
            let (vid, pid) = (hex(&ids, "VID_")?, hex(&ids, "PID_")?);
            let chain: Vec<u32> = paths
                .split(';')
                .next()?
                .split('#')
                .filter_map(|seg| {
                    seg.strip_prefix("USBROOT(").or_else(|| seg.strip_prefix("USB("))?.strip_suffix(')')?.parse().ok()
                })
                .collect();
            let serial = parts.next().unwrap_or("").to_string();
            (relevant(vid, pid) && !chain.is_empty()).then_some(UsbLoc { vid, pid, serial, chain })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOB: &[u16] = &[0x0202, 0x9996];

    #[test]
    fn linux_sysfs_names() {
        assert_eq!(sysfs_chain("1-2.1.4"), Some(vec![1, 2, 1, 4]));
        assert_eq!(sysfs_chain("3-1"), Some(vec![3, 1]));
        assert_eq!(sysfs_chain("usb1"), None); // root hub
        assert_eq!(sysfs_chain("1-2.1.4:1.0"), None); // interface
                                                      // base hub on bus 1 port 2, knob module two hubs further down: key "14"
        let locs = vec![
            UsbLoc { vid: HUB_VID, pid: HUB_PID, serial: String::new(), chain: sysfs_chain("1-2").unwrap() },
            UsbLoc { vid: MODULE_VID, pid: 0x0202, serial: "K1".into(), chain: sysfs_chain("1-2.1.4").unwrap() },
        ];
        assert_eq!(port_key(&locs, KNOB, "K1").as_deref(), Some("14"));
    }

    #[test]
    fn reads_macos_locations() {
        // Base hub on bus 0x20 port 1; inner hub on its port 4; knob on the inner hub's port 3;
        // M0064 two levels below the base hub.
        let text = r#"
+-o Root  <class IORegistryEntry, id 0x100000100>
  +-o USB2.0 Hub@20100000  <class IOUSBHostDevice>
  | |   "idProduct" = 9606
  | |   "idVendor" = 1659
  | |   "locationID" = 537919488
  | +-o USB2.0 Hub@20140000  <class IOUSBHostDevice>
  |   |   "idProduct" = 9606
  |   |   "idVendor" = 1659
  |   |   "locationID" = 538181632
  |   +-o MasterHUB Knob@20143000  <class IOUSBHostDevice>
  |   |     "USB Serial Number" = "KNOB1"
  |   |     "idProduct" = 514
  |   |     "idVendor" = 9494
  |   |     "locationID" = 538193920
  |   +-o MasterHUB Base@20141000  <class IOUSBHostDevice>
  |         "idProduct" = 502
  |         "idVendor" = 9494
  |         "locationID" = 538185728
  +-o Keyboard@01100000  <class IOUSBHostDevice>
        "idProduct" = 1
        "idVendor" = 1452
        "locationID" = 17825792
"#;
        let locs = parse_ioreg(text);
        assert_eq!(locs.len(), 4);
        assert_eq!(
            locs[2],
            UsbLoc { vid: MODULE_VID, pid: 0x0202, serial: "KNOB1".into(), chain: vec![0x20, 1, 4, 3] }
        );
        // relative to the outermost hub [0x20, 1]: ports 4, 3
        assert_eq!(port_key(&locs, KNOB, "KNOB1").as_deref(), Some("43"));
    }

    #[test]
    fn reads_windows_locations() {
        let text = "USB\\VID_067B&PID_2586\\5&1A&0&2|PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)\r\n\
                    USB\\VID_2516&PID_0202\\KNOB1|PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(2)#USB(2);ACPI(_SB_)\r\n\
                    USB\\VID_2516&PID_01FE&MI_00\\7&1A2B&0&0000|PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(4)#USBMI(0)\r\n";
        let locs = parse_pnp(text);
        assert_eq!(
            locs,
            vec![
                UsbLoc { vid: HUB_VID, pid: HUB_PID, serial: "5&1A&0&2".into(), chain: vec![0, 2] },
                UsbLoc { vid: MODULE_VID, pid: 0x0202, serial: "KNOB1".into(), chain: vec![0, 2, 2, 2] },
            ]
        );
        assert_eq!(port_key(&locs, KNOB, "knob1").as_deref(), Some("22")); // serial match ignores case
    }

    #[test]
    fn port_key_fallbacks() {
        let hub = UsbLoc { vid: HUB_VID, pid: HUB_PID, serial: String::new(), chain: vec![0, 5] };
        let knob = UsbLoc { vid: MODULE_VID, pid: 0x9996, serial: "6&ABC&0&3".into(), chain: vec![0, 5, 1, 4] };
        // second PID, no serial match but the only knob module -> still found
        assert_eq!(port_key(&[hub, knob], KNOB, "0").as_deref(), Some("14"));
        // not under a MasterHUB hub
        let lone = UsbLoc { vid: MODULE_VID, pid: 0x0202, serial: "X".into(), chain: vec![0, 3] };
        assert_eq!(port_key(&[lone], KNOB, "X"), None);
        assert_eq!(mac_chain(0x14130000), vec![0x14, 1, 3]);
    }
}
