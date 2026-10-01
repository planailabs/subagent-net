//! Images for models: tool results carry them as inline blobs, which the
//! agent's node stores in the hub's blob store and replaces by a marker
//! (`[image blob:<sha256> image/png 800x600]`). A model that can see
//! (`Spec.vision`) gets the latest ones attached to its calls, converted to a
//! format it takes and scaled down; others see only the marker.

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::Mutex;

use image::{ImageFormat, ImageReader};
use serde_json::Value;
use subnet_core::agent::Vision;
use subnet_core::chat::{Image, Message, Role};

/// Images this node stored lately (hash → mime, bytes), so attaching them to
/// the next calls needn't ask the hub.
// ponytail: a small process-wide FIFO; a byte-bounded LRU if nodes serve many seeing agents.
static RECENT: Mutex<VecDeque<(String, String, Vec<u8>)>> = Mutex::new(VecDeque::new());
const RECENT_MAX: usize = 32;

pub fn remember(hash: &str, mime: &str, bytes: &[u8]) {
    let mut r = RECENT.lock().unwrap();
    if r.iter().any(|(h, ..)| h == hash) {
        return;
    }
    if r.len() >= RECENT_MAX {
        r.pop_front();
    }
    r.push_back((hash.into(), mime.into(), bytes.to_vec()));
}

pub fn recent(hash: &str) -> Option<(String, Vec<u8>)> {
    RECENT.lock().unwrap().iter().find(|(h, ..)| h == hash).map(|(_, m, b)| (m.clone(), b.clone()))
}

/// Replaces the inline blobs of a tool result (lines `{"$blob": {base64,
/// mime}}`) by references: images (fitted for `vision`, if the agent sees)
/// by a marker, anything else by `[blob:<hash> <mime>]`. Returns the text and
/// the blobs to upload (hash, mime, base64).
pub fn store_inline(content: &str, vision: Option<&Vision>) -> (String, Vec<(String, String, String)>) {
    use base64::Engine;
    let b64 = &base64::engine::general_purpose::STANDARD;
    let mut uploads = vec![];
    let lines: Vec<String> = content
        .lines()
        .map(|line| {
            let Some(blob) = line.trim_start().starts_with("{\"$blob\"").then(|| serde_json::from_str::<Value>(line).ok()).flatten() else {
                return line.to_string();
            };
            let (Some(data), mime) = (blob["$blob"]["base64"].as_str(), blob["$blob"]["mime"].as_str().unwrap_or("application/octet-stream")) else {
                return line.to_string();
            };
            let Ok(bytes) = b64.decode(data) else { return "[a blob that wasn't valid base64]".into() };
            if mime.starts_with("image/") {
                let (bytes, mime, w, h) = match vision.map(|v| fit(&bytes, v)) {
                    Some(Ok(f)) => f,
                    // Not decodable, or no vision: keep it as it came.
                    _ => {
                        let (w, h) = ImageReader::new(Cursor::new(&bytes)).with_guessed_format().ok().and_then(|r| r.into_dimensions().ok()).unwrap_or((0, 0));
                        (bytes, mime.to_string(), w, h)
                    }
                };
                let hash = crate::hub::blobs::hash(&bytes);
                remember(&hash, &mime, &bytes);
                uploads.push((hash.clone(), mime.clone(), b64.encode(&bytes)));
                marker(&hash, &mime, w, h)
            } else {
                let hash = crate::hub::blobs::hash(&bytes);
                uploads.push((hash.clone(), mime.to_string(), data.to_string()));
                format!("[blob:{hash} {mime}]")
            }
        })
        .collect();
    (lines.join("\n"), uploads)
}

/// Replaces `blob:<sha256>` references in the arguments the tool's schema
/// marks as blobs (`"format": "blob"` on a string property, or on the items
/// of an array property) by `data:<mime>;base64,…` URLs. A reference that
/// can't be fetched is an error the model sees.
pub async fn resolve_refs<F, Fut>(args: &mut Value, schema: &Value, fetch: F) -> Result<(), String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Option<(String, Vec<u8>)>>,
{
    use base64::Engine;
    let Some(props) = schema["properties"].as_object() else { return Ok(()) };
    for (name, p) in props {
        let one = p["format"] == "blob";
        let many = p["items"]["format"] == "blob";
        if !one && !many {
            continue;
        }
        let Some(v) = args.get_mut(name) else { continue };
        let targets: Vec<&mut Value> = match v {
            Value::String(_) if one => vec![v],
            Value::Array(items) if many => items.iter_mut().collect(),
            _ => vec![],
        };
        for t in targets {
            let Some(r) = t.as_str() else { continue };
            let Some(hash) = r.strip_prefix("blob:") else { continue };
            let (mime, bytes) = fetch(hash.to_string()).await.ok_or_else(|| format!("{name}: no blob {r}"))?;
            *t = Value::String(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)));
        }
    }
    Ok(())
}

/// For a model that sees: the latest `keep` images whose markers are in the
/// conversation, attached where the model reads them. A user message
/// carries its own; a tool result's go in a user message after the run of
/// tool results (chat APIs take images only from users). `fetch` gets a
/// blob's mime and bytes; images it can't get stay markers.
pub async fn attach<F, Fut>(msgs: &mut Vec<Message>, v: &Vision, fetch: F)
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Option<(String, Vec<u8>)>>,
{
    use base64::Engine;
    let found: Vec<(usize, String)> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, Role::User | Role::Tool))
        .flat_map(|(i, m)| markers(m.content.as_deref().unwrap_or_default()).into_iter().map(move |(h, _)| (i, h)))
        .collect();
    let keep: Vec<(usize, String)> = found.into_iter().rev().take(v.keep).collect::<Vec<_>>().into_iter().rev().collect();
    // (message index to put them after or on, images); later indices first.
    let mut by_place: Vec<(usize, bool, Vec<Image>)> = vec![];
    for (i, hash) in keep {
        let Some((mime, bytes)) = (match recent(&hash) {
            Some(r) => Some(r),
            None => fetch(hash.clone()).await,
        }) else {
            continue;
        };
        // Images from events come as they were sent: fitted here.
        let Ok((bytes, mime, ..)) = fit(&bytes, v).inspect_err(|e| tracing::warn!(%hash, %mime, e, "an image the model can't take")) else {
            continue;
        };
        let img = Image { mime, base64: base64::engine::general_purpose::STANDARD.encode(bytes) };
        let (place, on_message) = if msgs[i].role == Role::User {
            (i, true)
        } else {
            let mut end = i;
            while end + 1 < msgs.len() && msgs[end + 1].role == Role::Tool {
                end += 1;
            }
            (end, false)
        };
        match by_place.iter_mut().find(|(p, on, _)| *p == place && *on == on_message) {
            Some((.., imgs)) => imgs.push(img),
            None => by_place.push((place, on_message, vec![img])),
        }
    }
    by_place.sort_by(|a, b| b.0.cmp(&a.0));
    for (place, on_message, images) in by_place {
        if on_message {
            msgs[place].images.extend(images);
        } else {
            let mut m = Message::user("[the images from the tool results above]");
            m.images = images;
            msgs.insert(place + 1, m);
        }
    }
}

/// The marker a stored image leaves in text.
pub fn marker(hash: &str, mime: &str, w: u32, h: u32) -> String {
    format!("[image blob:{hash} {mime} {w}x{h}]")
}

/// Every image marker in `text`: (blob hash, mime).
pub fn markers(text: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut rest = text;
    while let Some(i) = rest.find("[image blob:") {
        rest = &rest[i + "[image blob:".len()..];
        let Some(end) = rest.find(']') else { break };
        let mut parts = rest[..end].split(' ');
        if let (Some(h), Some(m)) = (parts.next(), parts.next())
            && h.len() == 64
            && h.bytes().all(|b| b.is_ascii_hexdigit())
        {
            out.push((h.to_string(), m.to_string()));
        }
        rest = &rest[end..];
    }
    out
}

fn format_of(name: &str) -> Option<ImageFormat> {
    match name {
        "png" => Some(ImageFormat::Png),
        "jpeg" | "jpg" => Some(ImageFormat::Jpeg),
        "webp" => Some(ImageFormat::WebP),
        "gif" => Some(ImageFormat::Gif),
        _ => None,
    }
}

/// An image as a model of `v` takes it: kept when its format is accepted
/// and it fits `max_px`, else decoded, scaled to fit and re-encoded (JPEG if
/// accepted, else PNG, else the first accepted format). Returns the bytes,
/// their mime and the size.
pub fn fit(bytes: &[u8], v: &Vision) -> Result<(Vec<u8>, String, u32, u32), String> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let format = reader.format().ok_or("not an image (unknown format)")?;
    let accepted: Vec<ImageFormat> = v.formats.iter().filter_map(|f| format_of(f)).collect();
    let (w, h) = reader.into_dimensions().map_err(|e| e.to_string())?;
    if accepted.contains(&format) && w.max(h) <= v.max_px {
        return Ok((bytes.to_vec(), format.to_mime_type().to_string(), w, h));
    }
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let img = if w.max(h) > v.max_px { img.resize(v.max_px, v.max_px, image::imageops::FilterType::Lanczos3) } else { img };
    let to = [ImageFormat::Jpeg, ImageFormat::Png].into_iter().find(|f| accepted.contains(f)).or(accepted.first().copied()).ok_or("the model accepts no image format")?;
    // JPEG has no alpha channel.
    let img = if to == ImageFormat::Jpeg { image::DynamicImage::ImageRgb8(img.to_rgb8()) } else { img };
    let mut out = Cursor::new(vec![]);
    img.write_to(&mut out, to).map_err(|e| e.to_string())?;
    Ok((out.into_inner(), to.to_mime_type().to_string(), img.width(), img.height()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Cursor::new(vec![]);
        image::DynamicImage::new_rgba8(w, h).write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn accepted_images_pass_others_are_converted_and_scaled() {
        let v = Vision::default();
        let (b, mime, w, h) = fit(&png(40, 20), &v).unwrap();
        assert_eq!((mime.as_str(), w, h), ("image/png", 40, 20));
        assert_eq!(b, png(40, 20), "untouched");
        // Too big: scaled to fit, aspect kept.
        let (_, _, w, h) = fit(&png(4000, 2000), &v).unwrap();
        assert_eq!((w, h), (1568, 784));
        // A format the model doesn't take: JPEG if it may, else PNG.
        let only_jpeg = Vision { formats: vec!["jpeg".into()], ..v.clone() };
        let (b, mime, ..) = fit(&png(10, 10), &only_jpeg).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(&b[..2], &[0xff, 0xd8]);
        let mut bmp = Cursor::new(vec![]);
        image::DynamicImage::new_rgb8(8, 8).write_to(&mut bmp, ImageFormat::Bmp).unwrap();
        let only_png = Vision { formats: vec!["png".into()], ..v };
        assert_eq!(fit(bmp.get_ref(), &only_png).unwrap().1, "image/png");
        assert!(fit(b"not an image", &only_png).is_err());
    }

    #[test]
    fn tool_results_store_images_and_models_get_the_latest() {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let line = |b: &[u8], mime: &str| serde_json::json!({"$blob": {"base64": b64.encode(b), "mime": mime}}).to_string();
        let content = format!("a view\n{}\n{}", line(&png(4000, 10), "image/png"), line(b"RIFF", "audio/wav"));
        let (text, uploads) = store_inline(&content, Some(&Vision::default()));
        assert_eq!(uploads.len(), 2);
        let m = markers(&text);
        assert_eq!(m.len(), 1, "{text}");
        assert!(text.contains("1568x4") && text.contains("[blob:") && text.contains("audio/wav"), "{text}");
        assert!(recent(&m[0].0).is_some(), "kept for the next call");

        // Two tool results with an image each, then a user message with one; keep 2.
        let mut call = Message::assistant("");
        call.tool_calls = vec![subnet_core::chat::ToolCall::new("1", "x", "{}"), subnet_core::chat::ToolCall::new("2", "x", "{}")];
        let img = |n: u8| {
            let b = png(n as u32 + 1, 1);
            let h = crate::hub::blobs::hash(&b);
            remember(&h, "image/png", &b);
            marker(&h, "image/png", n as u32 + 1, 1)
        };
        let mut msgs = vec![Message::system("s"), call, Message::tool("1", img(1)), Message::tool("2", img(2)), Message::user(format!("and {}", img(3)))];
        let v = Vision { keep: 2, ..Vision::default() };
        futures::executor::block_on(attach(&mut msgs, &v, |_| async { None }));
        assert_eq!(msgs.len(), 6, "one user message after the tool results");
        assert_eq!(msgs[4].role, Role::User);
        assert_eq!(msgs[4].images.len(), 1, "only the newest two in all: the second tool image");
        assert_eq!(msgs[5].images.len(), 1, "and the user's own");
        assert!(msgs[2].images.is_empty() && msgs[3].images.is_empty());
    }

    #[test]
    fn blob_arguments_become_data_urls() {
        let h = "b".repeat(64);
        let schema = serde_json::json!({"properties": {
            "attachments": {"type": "array", "items": {"type": "string", "format": "blob"}},
            "cover": {"type": "string", "format": "blob"},
            "title": {"type": "string"}}});
        let mut args = serde_json::json!({"attachments": [format!("blob:{h}"), "plain"], "cover": format!("blob:{h}"), "title": format!("blob:{h}")});
        futures::executor::block_on(resolve_refs(&mut args, &schema, |_| async { Some(("image/png".to_string(), b"hi".to_vec())) })).unwrap();
        assert_eq!(args["attachments"][0], "data:image/png;base64,aGk=");
        assert_eq!(args["attachments"][1], "plain");
        assert_eq!(args["cover"], "data:image/png;base64,aGk=");
        assert_eq!(args["title"], format!("blob:{h}"), "only properties marked as blobs");
        let mut missing = serde_json::json!({"cover": format!("blob:{h}")});
        assert!(futures::executor::block_on(resolve_refs(&mut missing, &schema, |_| async { None })).is_err());
    }

    #[test]
    fn markers_round_trip() {
        let h = "a".repeat(64);
        let text = format!("look: {} and {}", marker(&h, "image/png", 3, 4), "[image blob:short x]");
        assert_eq!(markers(&text), vec![(h, "image/png".to_string())]);
    }
}
