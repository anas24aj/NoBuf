use tauri::State;
use tauri::Manager;
use grammers_client::types::{Media, photo_sizes::PhotoSize};
use base64::{Engine as _, engine::general_purpose};
use crate::TelegramState;
use crate::bandwidth::BandwidthManager;
use crate::commands::utils::resolve_peer;

const PREVIEW_CACHE_MAX_FILES: usize = 30;
const PREVIEW_CACHE_MAX_TOTAL_BYTES: u64 = 80 * 1024 * 1024;

fn prune_preview_cache(cache_dir: &std::path::Path) {
    let read_dir = match std::fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut files: Vec<(std::path::PathBuf, std::time::SystemTime, u64)> = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            files.push((path, modified, meta.len()));
        }
    }
    files.sort_by_key(|(_, modified, _)| *modified);
    let mut total_bytes: u64 = files.iter().map(|(_, _, len)| *len).sum();
    while files.len() > PREVIEW_CACHE_MAX_FILES || total_bytes > PREVIEW_CACHE_MAX_TOTAL_BYTES {
        if let Some((path, _, len)) = files.first().cloned() {
            let _ = std::fs::remove_file(&path);
            total_bytes = total_bytes.saturating_sub(len);
            files.remove(0);
        } else {
            break;
        }
    }
}

#[tauri::command]
pub async fn cmd_get_preview(
    message_id: i32,
    folder_id: Option<i64>,
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
    bw_state: State<'_, BandwidthManager>,
) -> Result<String, String> {
    let cache_dir = app_handle
        .path()
        .app_cache_dir()
        .map_err(|e: tauri::Error| e.to_string())?
        .join("previews");
    if !cache_dir.exists() {
        let _ = std::fs::create_dir_all(&cache_dir);
    }
    prune_preview_cache(&cache_dir);
    log::info!("Using preview cache dir: {:?}", cache_dir);
    log::info!("Preview Request: msg_id={}", message_id);
    let client_opt = { state.client.lock().await.clone() };
    if client_opt.is_none() {
        return Ok("".to_string());
    }
    let client = client_opt.unwrap();

    let peer = resolve_peer(&client, folder_id, &state.peer_cache).await?;
    let messages = client.get_messages_by_id(&peer, &[message_id])
        .await.map_err(|e| e.to_string())?;
    let target_message = messages.into_iter().flatten().next();

    if let Some(msg) = target_message {
        if let Some(media) = msg.media() {
            let ext = match &media {
                Media::Document(d) => {
                    let mut e = std::path::Path::new(d.name())
                        .extension()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if e.is_empty() {
                        if let Some(mime) = d.mime_type() {
                            e = match mime {
                                "image/jpeg" => "jpg".to_string(),
                                "image/png" => "png".to_string(),
                                "video/mp4" => "mp4".to_string(),
                                _ => "bin".to_string(),
                            };
                        } else {
                            e = "bin".to_string();
                        }
                    }
                    e
                },
                Media::Photo(_) => "jpg".to_string(),
                _ => "bin".to_string(),
            };
            let folder_key = folder_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "home".to_string());
            let save_path = cache_dir.join(format!("{}_{}.{}", folder_key, message_id, ext));
            let save_path_str = save_path.to_string_lossy().to_string();

            let file_ready = if save_path.exists() {
                log::info!("File ({}) exists in cache.", message_id);
                true
            } else {
                let size = match &media {
                    Media::Document(d) => d.size() as u64,
                    Media::Photo(_) => 1024 * 1024,
                    _ => 0,
                };
                log::info!("Downloading preview... Size: {}", size);
                if let Err(e) = bw_state.can_transfer(size) {
                    log::warn!("Bandwidth limit hit for preview: {}", e);
                    false
                } else {
                    match client.download_media(&media, &save_path_str).await {
                        Ok(_) => {
                            log::info!("Preview download complete.");
                            bw_state.add_down(size);
                            prune_preview_cache(&cache_dir);
                            true
                        },
                        Err(e) => {
                            log::error!("Preview Download Error: {}", e);
                            false
                        }
                    }
                }
            };
            if file_ready {
                let lower_ext = ext.to_lowercase();
                if ["jpg", "jpeg", "png", "gif", "webp", "bmp", "svg"].contains(&lower_ext.as_str()) {
                    log::info!("Converting image to Base64...");
                    match std::fs::read(&save_path) {
                        Ok(bytes) => {
                            let b64 = general_purpose::STANDARD.encode(&bytes);
                            let mime = match lower_ext.as_str() {
                                "png" => "image/png",
                                "gif" => "image/gif",
                                "webp" => "image/webp",
                                "bmp" => "image/bmp",
                                "svg" => "image/svg+xml",
                                _ => "image/jpeg",
                            };
                            return Ok(format!("data:{};base64,{}", mime, b64));
                        },
                        Err(e) => {
                            log::error!("Failed to read file for base64: {}", e);
                            return Ok(save_path_str);
                        }
                    }
                }
                log::info!("Returning path preview: {}", save_path_str);
                return Ok(save_path_str);
            }
        }
    }
    Err("File not found or failed to download".to_string())
}

#[tauri::command]
pub async fn cmd_clean_cache(
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    // Clean preview cache
    let cache_dir = app_handle
        .path()
        .app_cache_dir()
        .map_err(|e: tauri::Error| e.to_string())?
        .join("previews");
    if cache_dir.exists() {
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    // Clean thumbnail cache (unbounded — can grow to hundreds of MBs)
    if let Ok(data_dir) = app_handle.path().app_data_dir() {
        let thumb_dir = data_dir.join("thumbnails");
        if thumb_dir.exists() {
            let _ = std::fs::remove_dir_all(&thumb_dir);
        }
    }

    // Clean stream cache (the big one — can be GBs of .dat files)
    if let Some(cache_mgr) = app_handle.try_state::<crate::stream_cache::StreamCacheManager>() {
        if let Err(e) = cache_mgr.clear_all_robust() {
            log::warn!("[cmd_clean_cache] clear_all_robust failed: {}", e);
            // Fallback to simple
            let _ = cache_mgr.clear_all();
        }
    }

    // Clean orphaned remux files in %TEMP%
    let temp_remux = std::env::temp_dir().join("nobuf_remux");
    if temp_remux.exists() {
        let _ = std::fs::remove_dir_all(&temp_remux);
    }

    Ok(())
}

// Keep grid requests from flooding Telegram when a channel has many videos.
static THUMBNAIL_DOWNLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

fn thumbnail_cache_prefix(folder_id: Option<i64>, message_id: i32) -> String {
    let peer = folder_id.map(|id| id.to_string()).unwrap_or_else(|| "saved".to_string());
    format!("{}_{}.", peer, message_id)
}

fn best_document_thumbnail(thumbs: Vec<PhotoSize>) -> Option<PhotoSize> {
    thumbs.into_iter()
        .filter(|thumb| !matches!(thumb, PhotoSize::Empty(_) | PhotoSize::Path(_)))
        .filter(|thumb| thumb.size() > 0 && thumb.size() <= 2 * 1024 * 1024)
        .max_by_key(|thumb| thumb.size())
}

#[cfg(test)]
mod thumbnail_tests {
    use super::*;
    use grammers_client::types::{media::Document, Downloadable};
    use grammers_tl_types as tl;

    fn video_thumbs(thumbs: Vec<tl::enums::PhotoSize>) -> Vec<PhotoSize> {
        Document::from_raw_media(tl::types::MessageMediaDocument {
            nopremium: false, spoiler: false, video: true, round: false, voice: false,
            document: Some(tl::types::Document {
                id: 123, access_hash: 456, file_reference: vec![1], date: 0,
                mime_type: "video/mp4".into(), size: 100_000_000,
                thumbs: Some(thumbs), video_thumbs: None, dc_id: 2, attributes: vec![],
            }.into()),
            alt_documents: None, video_cover: None, video_timestamp: None, ttl_seconds: None,
        }).thumbs()
    }

    #[test]
    fn uses_document_thumbnail_location_not_full_video() {
        let thumbs = video_thumbs(vec![
            tl::types::PhotoSize { r#type: "s".into(), w: 90, h: 90, size: 1000 }.into(),
            tl::types::PhotoSize { r#type: "m".into(), w: 320, h: 180, size: 9000 }.into(),
        ]);
        let thumb = best_document_thumbnail(thumbs).unwrap();
        match thumb.to_raw_input_location().unwrap() {
            tl::enums::InputFileLocation::InputDocumentFileLocation(location) => {
                assert_eq!(location.id, 123);
                assert_eq!(location.thumb_size, "m");
            }
            _ => panic!("expected a document thumbnail location"),
        }
    }

    #[test]
    fn skips_missing_vector_and_oversized_thumbnails() {
        assert!(best_document_thumbnail(video_thumbs(vec![])).is_none());
        let thumbs = video_thumbs(vec![
            tl::types::PhotoSizeEmpty { r#type: "s".into() }.into(),
            tl::types::PhotoPathSize { r#type: "j".into(), bytes: vec![1, 2] }.into(),
            tl::types::PhotoSize { r#type: "w".into(), w: 4000, h: 4000, size: 3_000_000 }.into(),
        ]);
        assert!(best_document_thumbnail(thumbs).is_none());
    }

    #[test]
    fn supports_inline_thumbnails_without_network_download() {
        let thumb = best_document_thumbnail(video_thumbs(vec![
            tl::types::PhotoCachedSize { r#type: "s".into(), w: 90, h: 90, bytes: vec![1, 2, 3] }.into(),
        ])).unwrap();
        assert_eq!(thumb.to_data(), Some(vec![1, 2, 3]));
    }

    #[test]
    fn separates_same_message_id_in_different_channels_and_saved_messages() {
        assert_ne!(thumbnail_cache_prefix(Some(1), 42), thumbnail_cache_prefix(Some(2), 42));
        assert_ne!(thumbnail_cache_prefix(None, 42), thumbnail_cache_prefix(Some(0), 42));
        assert!(!"1_420.jpg".starts_with(&thumbnail_cache_prefix(Some(1), 42)));
    }
}

/// Get an image or Telegram-provided video thumbnail for a file card.
/// Videos without a still thumbnail retain their file icon; never fetch the full video.
#[tauri::command]
pub async fn cmd_get_thumbnail(
    message_id: i32,
    folder_id: Option<i64>,
    app_handle: tauri::AppHandle,
    state: State<'_, TelegramState>,
) -> Result<String, String> {
    let _permit = THUMBNAIL_DOWNLOADS.acquire().await.map_err(|e| e.to_string())?;
    let cache_prefix = thumbnail_cache_prefix(folder_id, message_id);
    // Check if thumbnail already in cache
    let cache_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e: tauri::Error| e.to_string())?
        .join("thumbnails");
    if !cache_dir.exists() {
        let _ = std::fs::create_dir_all(&cache_dir);
    }

    // Check for any cached thumbnail for this message
    // Look for existing cached file
    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&cache_prefix) {
                // Found cached thumbnail, return as base64
                if let Ok(bytes) = std::fs::read(entry.path()) {
                    let ext = name.rsplit('.').next().unwrap_or("jpg");
                    let mime = match ext {
                        "png" => "image/png",
                        "gif" => "image/gif",
                        "webp" => "image/webp",
                        _ => "image/jpeg",
                    };
                    let b64 = general_purpose::STANDARD.encode(&bytes);
                    return Ok(format!("data:{};base64,{}", mime, b64));
                }
            }
        }
    }

    // No cache, need to fetch from Telegram
    let client_opt = { state.client.lock().await.clone() };
    if client_opt.is_none() {
        return Ok("".to_string());
    }
    let client = client_opt.unwrap();

    let peer = resolve_peer(&client, folder_id, &state.peer_cache).await?;
    let messages = client.get_messages_by_id(&peer, &[message_id])
        .await.map_err(|e| e.to_string())?;
    if let Some(m) = messages.into_iter().flatten().next() {
        if let Some(media) = m.media() {
            // Only get thumbnails for photos and documents with photo thumbnails
            let (is_image, ext) = match &media {
                Media::Photo(_) => (true, "jpg".to_string()),
                Media::Document(d) => {
                    let mime = d.mime_type().unwrap_or("");
                    if mime.starts_with("image/") {
                        let e = match mime {
                            "image/png" => "png",
                            "image/gif" => "gif",
                            "image/webp" => "webp",
                            _ => "jpg",
                        };
                        (true, e.to_string())
                    } else {
                        let Some(thumb) = best_document_thumbnail(d.thumbs()) else {
                            return Ok(String::new());
                        };
                        let bytes = tokio::time::timeout(std::time::Duration::from_secs(20), async {
                            let mut download = client.iter_download(&thumb);
                            let mut bytes = Vec::new();
                            while let Some(chunk) = download.next().await.map_err(|e| e.to_string())? {
                                if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                                    return Err("Thumbnail exceeds size limit".to_string());
                                }
                                bytes.extend_from_slice(&chunk);
                            }
                            Ok::<_, String>(bytes)
                        }).await.map_err(|_| "Thumbnail request timed out".to_string())??;
                        if bytes.is_empty() {
                            return Ok(String::new());
                        }
                        // Only complete previews enter the cache.
                        let _ = tokio::fs::write(cache_dir.join(format!("{}jpg", cache_prefix)), &bytes).await;
                        return Ok(format!("data:image/jpeg;base64,{}", general_purpose::STANDARD.encode(&bytes)));
                    }
                },
                _ => return Ok("".to_string()),
            };

            if is_image {
                // Get photo thumbnail (smallest size for speed)
                let save_path = cache_dir.join(format!("{}{}", cache_prefix, ext));
                let save_path_str = save_path.to_string_lossy().to_string();

                // Download the thumbnail/photo
                if client.download_media(&media, &save_path_str).await.is_ok() {
                    if let Ok(bytes) = std::fs::read(&save_path) {
                        let mime = match ext.as_str() {
                            "png" => "image/png",
                            "gif" => "image/gif",
                            "webp" => "image/webp",
                            _ => "image/jpeg",
                        };
                        let b64 = general_purpose::STANDARD.encode(&bytes);
                        return Ok(format!("data:{};base64,{}", mime, b64));
                    }
                }
            }
        }
    }

    Ok("".to_string())
}
