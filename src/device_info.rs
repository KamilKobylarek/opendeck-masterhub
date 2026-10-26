//! What a module tells about itself through the SDK's "Get_*" feature reports (docs/sdk-commands.md),
//! shown on the settings page and used to leave the device's own lighting alone.

use hidapi::HidDevice;

/// Feature report ids of one SDK family.
pub struct Ids {
    fw_base: u8, // firmware info for type 0..2 is fw_base + type
    serial: u8,
    info: u8,
    backlight: Option<u8>,
    led_mode: Option<u8>,
    led_brightness: Option<u8>,
}

/// Base, knobs, faders, rollers (CoolTEK_SDK_CM_M0).
pub const M0: Ids =
    Ids { fw_base: 0x2d, serial: 0x30, info: 0x31, backlight: None, led_mode: Some(0x34), led_brightness: Some(0x32) };
/// 15xKey (CoolTEK_SDK_CM_M4315).
pub const M4315: Ids =
    Ids { fw_base: 0x37, serial: 0x3a, info: 0x3b, backlight: Some(0x3c), led_mode: None, led_brightness: None };
/// Dial (CoolTEK_SDK_CM_M131E).
pub const M131E: Ids =
    Ids { fw_base: 0x47, serial: 0x4a, info: 0x4b, backlight: Some(0x4c), led_mode: None, led_brightness: None };

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub id: String,
    pub name: String,
    pub firmware: Vec<String>,
    pub serial: Option<String>,
    pub device_info: Option<String>,
    pub backlight: Option<u8>,
    pub led_modes: Vec<u8>,
    pub led_brightness: Vec<u8>,
}

pub fn read(device: &HidDevice, ids: &Ids, id: &str, name: &str) -> Info {
    let get = |report: u8| {
        let mut raw = [0u8; 0x20]; // 32 bytes, like the SDK
        raw[0] = report;
        device.get_feature_report(&mut raw).ok().map(|_| raw)
    };
    Info {
        id: id.to_string(),
        name: name.to_string(),
        firmware: (0..3).filter_map(|t| get(ids.fw_base + t).as_ref().and_then(|r| parse_firmware(r))).collect(),
        serial: get(ids.serial).as_ref().and_then(|r| parse_string(r)),
        device_info: get(ids.info)
            .map(|r| parse_bytes(&r).iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty()),
        backlight: ids.backlight.and_then(get).map(|r| r[2]),
        led_modes: ids.led_mode.and_then(get).map(|r| parse_bytes(&r)).unwrap_or_default(),
        led_brightness: ids.led_brightness.and_then(get).map(|r| parse_bytes(&r)).unwrap_or_default(),
    }
}

/// [id, len, version u32 LE, text (11 bytes)] (the SDK's Get_FW_Infos).
pub fn parse_firmware(raw: &[u8]) -> Option<String> {
    let version = u32::from_le_bytes(raw.get(2..6)?.try_into().ok()?);
    let text = String::from_utf8_lossy(raw.get(6..17)?).trim_matches(char::from(0)).trim().to_string();
    match (text.is_empty(), version) {
        (true, 0) => None,
        (true, v) => Some(format!("{v:#010x}")),
        (false, _) => Some(text),
    }
}

/// [id, len, text..]
pub fn parse_string(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(&parse_bytes(raw)).trim_matches(char::from(0)).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// [id, len, bytes..]
pub fn parse_bytes(raw: &[u8]) -> Vec<u8> {
    let len = raw.get(1).map_or(0, |l| *l as usize);
    raw.get(2..(2 + len).min(raw.len())).unwrap_or_default().to_vec()
}

/// The modules table of the settings page.
pub fn html(infos: &[Info]) -> String {
    if infos.is_empty() {
        return "<h2>Modules</h2><p>No module connected.</p>".to_string();
    }
    let dash = || "—".to_string();
    let list =
        |v: &[u8]| if v.is_empty() { dash() } else { v.iter().map(u8::to_string).collect::<Vec<_>>().join(", ") };
    let rows: String = infos
        .iter()
        .map(|i| {
            format!(
                "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td><td><code>{}</code></td><td>{}</td><td>{} / {}</td></tr>",
                esc(&i.name),
                esc(&i.id),
                i.serial.as_deref().map_or_else(dash, esc),
                if i.firmware.is_empty() { dash() } else { esc(&i.firmware.join(" / ")) },
                i.device_info.as_deref().map_or_else(dash, esc),
                i.backlight.map_or_else(dash, |b| b.to_string()),
                list(&i.led_modes),
                list(&i.led_brightness),
            )
        })
        .collect();
    format!(
        "<h2>Modules</h2><table><tr><th>Module</th><th>OpenDeck id</th><th>Serial</th><th>Firmware (types 0 / 1 / 2)</th>\
         <th>Device info</th><th>Screen brightness</th><th>LED effect / brightness</th></tr>{rows}</table>"
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_feature_reports() {
        let mut fw = [0u8; 32];
        fw[..6].copy_from_slice(&[0x38, 15, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(parse_firmware(&fw).as_deref(), Some("0x01020304"));
        fw[6..12].copy_from_slice(b"V1.2.3");
        assert_eq!(parse_firmware(&fw).as_deref(), Some("V1.2.3"));
        assert_eq!(parse_firmware(&[0x38, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), None);

        let serial = [0x3a, 6, b'A', b'B', b'1', b'2', 0, 0, 9];
        assert_eq!(parse_string(&serial).as_deref(), Some("AB12"));
        assert_eq!(parse_bytes(&[0x34, 3, 1, 1, 9, 7]), vec![1, 1, 9]);
        assert_eq!(parse_bytes(&[0x34, 40, 1]), vec![1]); // length beyond the buffer is cut
        assert!(html(&[Info { id: "mh-x".into(), name: "<b>".into(), ..Default::default() }]).contains("&lt;b&gt;"));
    }
}
