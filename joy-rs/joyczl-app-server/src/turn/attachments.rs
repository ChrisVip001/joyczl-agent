//! Attachments stage of the turn lifecycle.

use joyczl_protocol::{codes, ErrorObject, TurnStartParams};

use crate::Server;

/// Media types we accept for attachments, by file extension — the extension wins over the
/// client's hint (a renamed file is the user's problem; a wrong content-type is worse).
/// Only images ride as content blocks: other files keep the "path mentioned in text" route,
/// where the model can open them with read_file.
pub(super) fn media_type_for_extension(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// The hard cap per image (raw bytes). Mirrors the upload endpoint's 20 MiB; base64
/// inflates by 4/3, so this is also the honest ceiling for what one request can carry.
pub(super) const IMAGE_MAX_BYTES: usize = 20 * 1024 * 1024;

/// Turn staged uploads into image content blocks. Everything is checked up front —
/// containment, existence, media type, size — and any refusal fails the turn before it
/// starts, with `Error:`-quality guidance (the same honesty rules as the tools).
pub(super) fn resolve_attachments(
    server: &Server,
    params: &TurnStartParams,
    provider_id: &str,
    vision: bool,
) -> Result<Vec<joyczl_provider::ContentBlock>, ErrorObject> {
    let Some(attachments) = &params.attachments else {
        return Ok(Vec::new());
    };
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    let bad = |message: String| ErrorObject {
        code: codes::INVALID_PARAMS,
        message,
        data: None,
    };
    if !vision {
        return Err(bad(format!(
"The model on this configuration ({provider_id}) cannot take images. Declare vision for it: /model add <name> {provider_id} <model> --vision (the same as adding \"vision\": true to that entry in <home>/models.json); to make it true of the whole provider, add \"vision\": true to its row in <home>/models-catalog.json and restart."
        )));
    }
    let home = server.settings().home;
    let home_real = home
        .canonicalize()
        .map_err(|e| bad(format!("The home directory is unavailable: {e}")))?;
    let mut images = Vec::new();
    for attachment in attachments {
        let raw = home.join(&attachment.path);
        // Symlinks resolve first: the real path must stay inside home, no exceptions.
        let path = raw.canonicalize().map_err(|_| {
            bad(format!(
                "Attachment is missing or unreadable: {}",
                attachment.path
            ))
        })?;
        if !path.starts_with(&home_real) {
            return Err(bad(format!(
                "Attachment {} is outside home — an attachment may only reference a file under the upload directory (<home>/uploads/…).",
                attachment.path
            )));
        }
        let media_type = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(media_type_for_extension)
            .map(str::to_string)
            .or_else(|| {
                attachment
                    .media_type
                    .as_deref()
                    .filter(|m| m.starts_with("image/"))
                    .map(str::to_string)
            })
            .ok_or_else(|| {
                bad(format!(
"Attachment {} is not a recognised image type (png / jpg / jpeg / gif / webp); for any other file, put the path in the message and the model will open it with read_file.",
                    attachment.path
                ))
            })?;
        let bytes =
            std::fs::read(&path).map_err(|e| bad(format!("Could not read the attachment: {e}")))?;
        if bytes.len() > IMAGE_MAX_BYTES {
            return Err(bad(format!(
                "Attachment {} is {} bytes, over the per-image limit of {IMAGE_MAX_BYTES}.",
                attachment.path,
                bytes.len()
            )));
        }
        // Pixel dimensions have to be measured too: a tall screenshot (1120×11100 and the like) is
        // barely 2 MB of bytes, yet the far end rejects it with a bare "unsupported image" — no
        // reason, no limit — so measure locally first.
        let (width, height) = crate::image_meta::size(&bytes).ok_or_else(|| {
            bad(format!(
"Attachment {} has no readable dimensions — its header is not a complete png / jpg / gif / webp, so it is probably corrupt, or only had its extension changed.",
                attachment.path
            ))
        })?;
        if width > crate::image_meta::MAX_EDGE || height > crate::image_meta::MAX_EDGE {
            return Err(bad(format!(
"Attachment {} is {width}×{height}, one side over the local conservative limit of {}: a remote vision model would refuse it outright (a bare \"unsupported image\"), so it is caught here. Send a long screenshot in pieces — a few images no taller than {} keep the text legible; squeezing it into one only blurs it.",
                attachment.path,
                crate::image_meta::MAX_EDGE,
                crate::image_meta::MAX_EDGE
            )));
        }
        use base64::Engine as _;
        images.push(joyczl_provider::ContentBlock::Image {
            media_type: media_type.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
    }
    Ok(images)
}
