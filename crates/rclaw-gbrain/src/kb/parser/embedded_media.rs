use crate::kb::types::MediaRef;
use base64::Engine;

pub(crate) fn embedded_image_ref(storage_path: String, bytes: Vec<u8>) -> Option<MediaRef> {
    if bytes.is_empty() {
        return None;
    }
    let mime_type = image_mime_for(&storage_path, &bytes).map(str::to_string);
    let embedded_data_base64 = Some(base64::engine::general_purpose::STANDARD.encode(&bytes));
    Some(MediaRef {
        media_type: "image".to_string(),
        storage_path,
        mime_type,
        byte_size: Some(bytes.len() as i64),
        embedded_data_base64,
        alt_text: None,
        ocr_text: None,
        caption: None,
        page_number: None,
    })
}

fn image_mime_for(path: &str, bytes: &[u8]) -> Option<&'static str> {
    if let Some(kind) = infer::get(bytes) {
        let mime = kind.mime_type();
        if mime.starts_with("image/") {
            return Some(mime);
        }
    }

    match path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("bmp") | Some("dib") => Some("image/bmp"),
        Some("tif") | Some("tiff") => Some("image/tiff"),
        Some("emf") => Some("image/emf"),
        Some("wmf") => Some("image/wmf"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_image_ref_keeps_bytes_out_of_serialized_metadata() {
        let media = embedded_image_ref(
            "embedded://docx/word/media/image1.png".to_string(),
            vec![0x89, b'P', b'N', b'G'],
        )
        .expect("media");

        let json = serde_json::to_value(&media).expect("serialize");
        assert_eq!(json["media_type"], "image");
        assert!(json.get("embedded_data_base64").is_none());
        assert!(media.embedded_data_base64.is_some());
    }
}
