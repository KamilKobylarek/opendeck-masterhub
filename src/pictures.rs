//! Pictures for the modules: key images, the dial's screen and the 15xKey background (still or animated), as
//! the JPEGs the firmware takes, turned upright for turned modules.

use super::*;

/// The 15xKey full-screen background: the first picture as a JPEG for Display_Picture_On_Screen (02 35) and,
/// for an animated GIF, every frame with how long it stays.
pub(crate) struct KeypadBackground {
    pub(crate) first: Vec<u8>,
    pub(crate) frames: Vec<(RgbImage, Duration)>,
}

/// Animation limits: the 15xKey shows ~32 full-screen pictures a second, every one of them (checked 2026-10-28:
/// a 32-frame numbered GIF filmed at 120 fps, the plugin sending 31–32 a second, no frame missed, no reset).
pub(crate) const MIN_FRAME: Duration = Duration::from_millis(31);

/// The shortest a background frame stays: MIN_FRAME, or MASTERHUB_MIN_FRAME_MS (10–1000) set before OpenDeck
/// starts, to try another rate on a module.
pub(crate) fn min_frame() -> Duration {
    static MIN: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("MASTERHUB_MIN_FRAME_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(MIN_FRAME, |ms| Duration::from_millis(ms.clamp(10, 1000)))
    })
}
pub(crate) const MAX_FRAMES: usize = 300; // 480x272 RGB each, ~120 MB at most

/// The keypad background, or None when unset or unreadable.
/// The keypad background, decoded once per file (path, change time, size: an upload keeps its path) on a thread of
/// its own, since a big GIF takes seconds; None until it's ready (then `Up::BackgroundChanged` sends it out) or
/// when it can't be read.
pub(crate) fn keypad_background(cfg: &Config, up_tx: &Sender<Up>) -> Option<Arc<KeypadBackground>> {
    type Cache = Option<((String, Option<std::time::SystemTime>, u64), Option<Arc<KeypadBackground>>)>;
    static CACHE: Mutex<Cache> = Mutex::new(None);
    let path = cfg.keypad_background.clone()?;
    let meta = std::fs::metadata(&path).ok();
    let key = (path.clone(), meta.as_ref().and_then(|m| m.modified().ok()), meta.as_ref().map_or(0, |m| m.len()));
    let mut cache = CACHE.lock().unwrap();
    if let Some((cached, bg)) = cache.as_ref().filter(|(k, _)| *k == key) {
        let _ = cached;
        return bg.clone();
    }
    *cache = Some((key.clone(), None));
    let up_tx = up_tx.clone();
    std::thread::spawn(move || match load_background(&path) {
        Ok(bg) => {
            let mut cache = CACHE.lock().unwrap();
            if cache.as_ref().is_some_and(|(k, _)| *k == key) {
                *cache = Some((key, Some(Arc::new(bg))));
                let _ = up_tx.send(Up::BackgroundChanged);
            }
        }
        Err(e) => eprintln!("keypad background {path}: {e}"),
    });
    None
}

/// Limits for decoding a picture the plugin is given (a background file, an action's image): no bigger than
/// 4096×4096 and 256 MB, so a small file claiming a huge picture can't take the memory.
pub(crate) fn picture_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(256 << 20);
    limits
}

pub(crate) fn load_background(path: &str) -> Result<KeypadBackground, Box<dyn std::error::Error>> {
    let fit = |img: DynamicImage| {
        img.resize_to_fill(KEYPAD_SCREEN.0, KEYPAD_SCREEN.1, image::imageops::FilterType::Triangle).to_rgb8()
    };
    let mut frames = Vec::new();
    if std::fs::metadata(path)?.len() > 64 << 20 {
        return Err("bigger than 64 MB".into());
    }
    if path.to_ascii_lowercase().ends_with(".gif") {
        use image::{AnimationDecoder, ImageDecoder};
        let mut decoder = image::codecs::gif::GifDecoder::new(BufReader::new(std::fs::File::open(path)?))?;
        decoder.set_limits(picture_limits())?;
        for frame in decoder.into_frames().take(MAX_FRAMES) {
            let frame = frame?;
            let delay = Duration::from(frame.delay()).max(min_frame());
            frames.push((fit(DynamicImage::ImageRgba8(frame.into_buffer())), delay));
        }
    }
    let first = match frames.first() {
        Some((img, _)) => DynamicImage::from(img.clone()),
        None => {
            let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
            reader.limits(picture_limits());
            DynamicImage::from(fit(reader.decode()?))
        }
    };
    if frames.len() < 2 {
        frames.clear(); // a still picture
    }
    Ok(KeypadBackground { first: to_jpeg(&first)?, frames })
}

/// Whether pictures are turned upright on a module turned by `rot`.
pub(crate) fn upright(shared: &Shared, rot: u16) -> bool {
    rot != 0 && shared.read().unwrap().upright
}

/// A JPEG turned against a module's clockwise rotation `rot`, so it stands upright on the turned module.
pub(crate) fn turn_jpeg(jpeg: &[u8], rot: u16) -> Vec<u8> {
    let Ok(img) = image::load_from_memory(jpeg) else { return jpeg.to_vec() };
    let turned = match rot % 360 {
        90 => img.rotate270(),
        180 => img.rotate180(),
        270 => img.rotate90(),
        _ => img,
    };
    to_jpeg(&turned).unwrap_or_else(|_| jpeg.to_vec())
}

/// Where key `k` (module order) shows in the full-screen picture: a 79x75 window, columns 100 px and rows
/// 99 px apart from (0, 0).
pub(crate) const KEY_WINDOW: (u32, u32) = (79, 75);
pub(crate) fn key_window(k: u8) -> (i64, i64) {
    ((k % KEY_COLS) as i64 * 100, (k / KEY_COLS) as i64 * 99)
}

/// One animation frame with the keys' own images pasted into their windows, so a frame is one write.
pub(crate) fn panel_frame(frame: &RgbImage, keys: &HashMap<u8, RgbImage>) -> RgbImage {
    let mut canvas = frame.clone();
    for (&k, img) in keys {
        let (x, y) = key_window(k);
        image::imageops::overlay(&mut canvas, img, x, y);
    }
    canvas
}

pub(crate) fn decode_data_uri(data_uri: &str) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    let b64 = data_uri.split_once(',').map_or(data_uri, |(_, b)| b);
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    reader.limits(picture_limits());
    Ok(reader.decode()?)
}

pub(crate) fn to_jpeg(img: &DynamicImage) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut Cursor::new(&mut out), 90).encode_image(&img.to_rgb8())?;
    Ok(out)
}

/// The keys only accept JPEG, so re-encode whatever OpenDeck sends (a data URI) at key size, plus the
/// "pressed" look of the official app: the icon shrunk to 36x36 at (20,20) on black.
pub(crate) fn to_key_jpegs(data_uri: &str) -> Result<(Vec<u8>, Vec<u8>), Box<dyn std::error::Error>> {
    let img = decode_data_uri(data_uri)?;
    let normal = img.resize_exact(KEY_IMAGE_SIZE, KEY_IMAGE_SIZE, image::imageops::FilterType::Lanczos3);
    let mut pressed = RgbImage::new(KEY_IMAGE_SIZE, KEY_IMAGE_SIZE);
    image::imageops::overlay(
        &mut pressed,
        &img.resize_exact(36, 36, image::imageops::FilterType::Lanczos3).to_rgb8(),
        20,
        20,
    );
    Ok((to_jpeg(&normal)?, to_jpeg(&pressed.into())?))
}

/// OpenDeck renders encoder images as 200x100 strips; letterbox them onto the round 240x240 screen.
pub(crate) fn to_screen_jpeg(data_uri: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let img =
        decode_data_uri(data_uri)?.resize(SCREEN_SIZE, SCREEN_SIZE, image::imageops::FilterType::Lanczos3).to_rgb8();
    let mut screen = RgbImage::new(SCREEN_SIZE, SCREEN_SIZE);
    image::imageops::overlay(
        &mut screen,
        &img,
        ((SCREEN_SIZE - img.width()) / 2).into(),
        ((SCREEN_SIZE - img.height()) / 2).into(),
    );
    to_jpeg(&screen.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animated_background_and_key_windows() {
        use image::codecs::gif::GifEncoder;
        let path = std::env::temp_dir().join(format!("mh-test-{}.gif", std::process::id()));
        {
            let mut enc = GifEncoder::new(std::fs::File::create(&path).unwrap());
            for colour in [[255, 0, 0, 255], [0, 0, 255, 255]] {
                let img = image::RgbaImage::from_pixel(48, 27, image::Rgba(colour));
                // 10 ms asked for: the plugin slows it down to MIN_FRAME
                enc.encode_frame(image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(10, 1)))
                    .unwrap();
            }
        }
        let bg = load_background(path.to_str().unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(bg.frames.len(), 2);
        assert!(bg.frames.iter().all(|(img, d)| img.dimensions() == KEYPAD_SCREEN && *d == MIN_FRAME));
        assert!(bg.frames[1].0.get_pixel(240, 136)[2] > 200); // second frame blue
                                                              // key 6 (row 1, column 1) lands in its window at (100, 99)
        let keys = HashMap::from([(6, RgbImage::from_pixel(KEY_WINDOW.0, KEY_WINDOW.1, image::Rgb([0, 255, 0])))]);
        let panel = panel_frame(&bg.frames[0].0, &keys);
        assert_eq!(panel.get_pixel(100, 99).0, [0, 255, 0]);
        assert_eq!(panel.get_pixel(178, 173).0, [0, 255, 0]);
        assert!(panel.get_pixel(99, 99)[0] > 200); // just outside: background red
    }

    #[test]
    fn jpegs_carry_the_jfif_header_the_firmware_checks() {
        // the modules only accept JFIF (FF E0 at offset 2) or BMP
        let jpeg = to_jpeg(&DynamicImage::from(RgbImage::new(72, 72))).unwrap();
        assert_eq!(&jpeg[..4], &[0xff, 0xd8, 0xff, 0xe0]);
    }

    fn png_uri(w: u32, h: u32) -> String {
        let mut png = Vec::new();
        RgbImage::new(w, h).write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))
    }

    #[test]
    fn pictures_turn_upright() {
        // a picture with a white top-left corner, turned for a module rotated 90° clockwise
        let mut img = RgbImage::new(72, 72);
        for x in 0..10 {
            for y in 0..10 {
                img.put_pixel(x, y, image::Rgb([255, 255, 255]));
            }
        }
        let turned =
            image::load_from_memory(&turn_jpeg(&to_jpeg(&DynamicImage::from(img)).unwrap(), 90)).unwrap().to_rgb8();
        // counter-clockwise: the top-left corner moves to the bottom-left
        assert!(turned.get_pixel(3, 68)[0] > 200);
        assert!(turned.get_pixel(3, 3)[0] < 60);
    }

    #[test]
    fn makes_images() {
        let (jpeg, pressed) = to_key_jpegs(&png_uri(144, 144)).unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        assert_eq!(image::load_from_memory(&jpeg).unwrap().width(), KEY_IMAGE_SIZE);
        assert_eq!(image::load_from_memory(&pressed).unwrap().width(), KEY_IMAGE_SIZE);

        // a 200x100 encoder strip becomes a 240x240 screen
        let screen = image::load_from_memory(&to_screen_jpeg(&png_uri(200, 100)).unwrap()).unwrap();
        assert_eq!((screen.width(), screen.height()), (SCREEN_SIZE, SCREEN_SIZE));
    }
}
