//! QR codes.
//!
//! Two output shapes, because they are wanted in two different places: a **PNG** for a
//! download or an upload, and an **SVG** for a page, where it stays sharp at any size and
//! weighs a fraction as much.
//!
//! ```no_run
//! # use rust_web_frame::component::qrcode;
//! # use rust_web_frame::web::{Request, Response, Result};
//! async fn ticket(req: Request) -> Result<Response> {
//!     qrcode::svg_response(req.query_or("code", "empty"))
//! }
//! ```
//!
//! A data URI is provided too, for the case where the image has to live inside a JSON
//! response rather than behind its own URL.

use base64::Engine as _;
use qrcode::QrCode;
use qrcode::render::{svg, unicode};
use serde::{Deserialize, Serialize};

use crate::web::{Error, Result};

/// Config section that holds the QR code settings.
pub const QRCODE_CONFIG_KEY: &str = "qrcode";

/// How a QR code is drawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct QrcodeConfig {
    /// Pixels per module in the PNG.
    pub scale: u32,
    /// Quiet zone around the code, in modules. The specification requires four, and a
    /// scanner that cannot find the edge will simply fail.
    pub margin: u32,
    /// Colour of the dark modules, as `#rrggbb`.
    pub dark: String,
    /// Colour of the light modules.
    pub light: String,
}

impl Default for QrcodeConfig {
    fn default() -> Self {
        Self {
            scale: 8,
            margin: 4,
            dark: String::from("#000000"),
            light: String::from("#ffffff"),
        }
    }
}

/// Encodes `content` as a QR code.
///
/// # Errors
///
/// Fails when the content is too long for a QR code — 2 953 bytes at the highest error
/// correction, less at the default.
pub fn encode(content: &str) -> Result<QrCode> {
    QrCode::new(content.as_bytes()).map_err(|err| {
        Error::new(400, "that cannot be encoded as a QR code")
            .caused_by(std::io::Error::other(err.to_string()))
    })
}

/// A QR code as an SVG response.
///
/// # Errors
///
/// Fails when the content cannot be encoded. The response is built by the caller, so a
/// failure here is a handler error like any other.
pub fn svg_response(content: &str) -> Result<crate::web::Response> {
    let code = encode(content)?;
    let image = code.render::<svg::Color>().min_dimensions(200, 200).build();

    Ok(crate::web::Response::text(image).with_header("content-type", "image/svg+xml"))
}

/// A QR code as a PNG response.
///
/// # Errors
///
/// Fails when the content cannot be encoded, or the configured colours are not colours.
pub fn png_response(content: &str, config: &QrcodeConfig) -> Result<crate::web::Response> {
    let png = png(content, config)?;
    Ok(crate::web::Response::raw(
        axum::response::Response::builder()
            .status(200)
            .header("content-type", "image/png")
            .body(axum::body::Body::from(png))
            .map_err(Error::from)?,
    ))
}

/// A QR code as PNG bytes.
///
/// # Errors
///
/// Fails when the content cannot be encoded, or a colour cannot be parsed.
pub fn png(content: &str, config: &QrcodeConfig) -> Result<Vec<u8>> {
    let code = encode(content)?;
    let dark = parse_colour(&config.dark)?;
    let light = parse_colour(&config.light)?;

    let image = code
        .render::<image::Luma<u8>>()
        .quiet_zone(config.margin > 0)
        .module_dimensions(config.scale.max(1), config.scale.max(1))
        .dark_color(image::Luma([luminance(&dark)]))
        .light_color(image::Luma([luminance(&light)]))
        .build();

    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .map_err(|err| Error::with_source(500, err).detail("cannot encode the PNG"))?;
    Ok(bytes)
}

/// A QR code as a `data:` URI, for embedding in a JSON payload.
///
/// # Errors
///
/// Fails when the content cannot be encoded.
pub fn data_uri(content: &str) -> Result<String> {
    let code = encode(content)?;
    let image = code
        .render::<image::Luma<u8>>()
        .quiet_zone(true)
        .module_dimensions(8, 8)
        .build();

    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .map_err(|err| Error::with_source(500, err).detail("cannot encode the PNG"))?;

    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// A QR code drawn with block characters, for a terminal or a log line.
///
/// # Errors
///
/// Fails when the content cannot be encoded.
pub fn terminal(content: &str) -> Result<String> {
    let code = encode(content)?;
    Ok(code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build())
}

/// Parses `#rrggbb`, `#rgb` or `rrggbb`.
fn parse_colour(text: &str) -> Result<[u8; 3]> {
    let hex = text.trim().trim_start_matches('#');
    let expanded = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => hex.to_owned(),
        _ => {
            return Err(Error::new(500, format!("`{text}` is not a colour"))
                .detail("colours are written as `#rrggbb`"));
        }
    };

    let parse = |from: usize| {
        u8::from_str_radix(&expanded[from..from + 2], 16)
            .map_err(|_| Error::new(500, format!("`{text}` is not a colour")))
    };
    Ok([parse(0)?, parse(2)?, parse(4)?])
}

/// A QR code is black and white; a dark colour is one the scanner reads as "set".
fn luminance(colour: &[u8; 3]) -> u8 {
    // Rec. 601 luma, which is what a scanner's threshold approximates.
    let luma =
        0.299 * f32::from(colour[0]) + 0.587 * f32::from(colour[1]) + 0.114 * f32::from(colour[2]);
    // Pure black or pure white: a mid-grey QR code is unreadable, and the configured colour
    // is a hint about the theme rather than a literal value.
    if luma < 128.0 { 0 } else { 255 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_becomes_a_code() {
        let code = encode("https://example.com/t/abc123").unwrap();
        assert!(!code.to_colors().is_empty());
    }

    #[test]
    fn more_content_makes_a_bigger_code() {
        let small = encode("a").unwrap();
        let large = encode("a much longer payload that has to fit in more modules").unwrap();
        assert!(large.width() > small.width());
    }

    #[test]
    fn content_too_long_is_a_client_error_not_a_panic() {
        let error = match encode(&"x".repeat(10_000)) {
            Ok(_) => panic!("ten kilobytes cannot fit in a QR code"),
            Err(error) => error,
        };
        assert_eq!(
            error.http_status(),
            400,
            "the caller asked for something impossible"
        );
    }

    #[test]
    fn an_svg_is_a_page_ready_image() {
        let response = svg_response("hello").unwrap();
        assert_eq!(
            response.header("content-type").unwrap(),
            "image/svg+xml",
            "a browser will not render it as an image otherwise"
        );

        match response.body() {
            crate::web::ResponseBody::Text(svg) => {
                assert!(
                    svg.contains("<svg"),
                    "an SVG document carries an <svg> element"
                );
                assert!(svg.contains("</svg>"));
            }
            other => panic!("expected SVG text, got {other:?}"),
        }
    }

    #[test]
    fn a_png_is_a_png() {
        let bytes = png("hello", &QrcodeConfig::default()).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "the PNG magic number");
    }

    #[test]
    fn a_data_uri_carries_a_png() {
        let uri = data_uri("hello").unwrap();
        let payload = uri
            .strip_prefix("data:image/png;base64,")
            .expect("the data URI prefix");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("valid base64");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn a_terminal_code_is_printable() {
        let drawn = terminal("hello").unwrap();
        assert!(drawn.lines().count() > 5, "{drawn}");
        assert!(!drawn.contains('\u{0}'));
    }

    #[test]
    fn colours_parse_in_the_shapes_a_person_writes() {
        assert_eq!(parse_colour("#000000").unwrap(), [0, 0, 0]);
        assert_eq!(parse_colour("#fff").unwrap(), [255, 255, 255]);
        assert_eq!(parse_colour("1a2b3c").unwrap(), [0x1a, 0x2b, 0x3c]);
        assert!(parse_colour("not a colour").is_err());
        assert!(parse_colour("#12345").is_err());
    }

    #[test]
    fn a_colour_becomes_the_nearest_of_black_and_white() {
        // A scanner thresholds, so a "dark blue" brand colour has to come out black rather
        // than as an unreadable grey.
        assert_eq!(luminance(&parse_colour("#000080").unwrap()), 0);
        assert_eq!(luminance(&parse_colour("#fffff0").unwrap()), 255);
        assert_eq!(luminance(&parse_colour("#333").unwrap()), 0);
        assert_eq!(luminance(&parse_colour("#ccc").unwrap()), 255);
    }
}
