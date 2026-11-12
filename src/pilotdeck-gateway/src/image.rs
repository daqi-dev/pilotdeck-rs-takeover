//! Native image validation and budget compression through pinned libvips.
use crate::native_component::{self, Error, Snapshot};
use std::time::Duration;
use tokio::process::Command;

pub(crate) const MAX_INPUT: usize = 128 * 1024 * 1024;
const MAX_OUTPUT: usize = 32 * 1024 * 1024;
pub(crate) const UNAVAILABLE: &str = "Image capability unavailable: install the libvips component (pilotdeck-image) or set PILOTDECK_IMAGE_TOOL.";

pub(crate) struct Image {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mime: &'static str,
}
fn error(error: Error) -> String {
    match error {
        Error::Unavailable => UNAVAILABLE.into(),
        Error::Failed(message) => message,
        Error::Timeout => "Image operation exceeded its 30 second time limit.".into(),
        Error::TooLarge => "Image operation exceeded its output size limit.".into(),
        Error::Cancelled => "Image operation was cancelled.".into(),
    }
}
fn decode(output: Vec<u8>) -> Result<Image, String> {
    let Some(split) = output.iter().position(|byte| *byte == b'\n') else {
        return Err("Image component returned an invalid response.".into());
    };
    let mime = match &output[..split] {
        b"image/jpeg" => "image/jpeg",
        b"image/png" => "image/png",
        b"image/gif" => "image/gif",
        b"image/webp" => "image/webp",
        _ => return Err("Image component returned an unsupported format.".into()),
    };
    if output.len() == split + 1 {
        return Err("Image component returned empty image data.".into());
    }
    Ok(Image {
        bytes: output[split + 1..].to_vec(),
        mime,
    })
}

fn decode_component(output: Vec<u8>, budget: usize, original_mime: &str) -> Result<Image, String> {
    let image = decode(output)?;
    if image.bytes.len() > MAX_OUTPUT {
        return Err("Image operation exceeded its output size limit.".into());
    }
    if image.bytes.len() > budget {
        return Err(format!(
            "Image content exceeds the read_file byte budget after compression attempts ({original_mime})."
        ));
    }
    Ok(image)
}

async fn process(bytes: &[u8], mime: &str, budget: usize, mode: &str) -> Result<Image, String> {
    if bytes.len() > MAX_INPUT {
        return Err("Image exceeds the native component input size limit.".into());
    }
    let input = Snapshot::new("image", bytes, MAX_INPUT).map_err(error)?;
    let mut command = Command::new(native_component::tool_path(
        "PILOTDECK_IMAGE_TOOL",
        "image",
        "pilotdeck-image",
    ));
    command
        .arg(mode)
        .arg(input.path())
        .arg(mime)
        .arg(budget.to_string());
    decode_component(
        native_component::run(
            command,
            Some(input),
            MAX_OUTPUT + 32,
            Duration::from_secs(30),
        )
        .await
        .map_err(error)?,
        budget,
        mime,
    )
}
pub(crate) async fn prepare(bytes: &[u8], mime: &str, budget: usize) -> Result<Image, String> {
    if mime == "image/jpeg"
        && bytes.len() > 1000
        && bytes.ends_with(&[0xff, 0xd9])
        && bytes.len() <= budget
    {
        return Ok(Image {
            bytes: bytes.to_vec(),
            mime: "image/jpeg",
        });
    }
    process(bytes, mime, budget, "prepare").await
}
pub(crate) async fn compress(bytes: &[u8], mime: &str, budget: usize) -> Result<Image, String> {
    if bytes.len() <= budget {
        return decode([format!("{mime}\n").into_bytes(), bytes.to_vec()].concat());
    }
    process(bytes, mime, budget, "compress").await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_component_output_cannot_become_an_image() {
        assert!(decode(b"image/jpeg".to_vec()).is_err());
        assert!(decode(b"text/plain\nbody".to_vec()).is_err());
        assert!(decode(b"image/jpeg\n".to_vec()).is_err());
        let image = decode(b"image/jpeg\n\xff\xd8\xff".to_vec()).unwrap();
        assert_eq!(image.mime, "image/jpeg");
        assert_eq!(image.bytes, b"\xff\xd8\xff");
        assert!(decode_component(b"image/jpeg\n1234".to_vec(), 3, "image/png").is_err());
        assert_eq!(
            decode_component(b"image/jpeg\n123".to_vec(), 3, "image/png")
                .unwrap()
                .bytes,
            b"123"
        );
    }
}
