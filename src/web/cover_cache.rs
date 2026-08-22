use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use reqwest::Client;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{
    config::AppConfig,
    error::{AppError, Result},
    repository::Repository,
};

const SERVER_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_COVER_BYTES: usize = 5 * 1024 * 1024;
const MAX_SUBJECT_PAGE_BYTES: usize = 2 * 1024 * 1024;
const FALLBACK_DELAY: Duration = Duration::from_millis(400);

#[derive(Clone)]
pub(super) struct CoverCache {
    directory: PathBuf,
    api_base_url: String,
    client: Client,
    request_timeout: Duration,
    fallback_request_timeout: Duration,
    filesystem_guard: Arc<RwLock<()>>,
}

pub(super) struct CoverAsset {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub etag: String,
}

impl CoverCache {
    pub fn new(config: &AppConfig) -> Result<Self> {
        let timeout_secs = config
            .web
            .request_timeout_secs
            .saturating_sub(1)
            .max(1)
            .min(config.schedule.request_timeout_secs);
        let fallback_timeout_secs = (timeout_secs / 2).clamp(1, 6);
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .user_agent(&config.schedule.user_agent)
            .build()
            .map_err(|error| {
                AppError::Config(format!("cannot build Bangumi cover client: {error}"))
            })?;
        Ok(Self {
            directory: PathBuf::from(&config.web.cover_cache_dir),
            api_base_url: config
                .schedule
                .bangumi_api_base_url
                .trim_end_matches('/')
                .into(),
            client,
            request_timeout: Duration::from_secs(timeout_secs),
            fallback_request_timeout: Duration::from_secs(fallback_timeout_secs),
            filesystem_guard: Arc::new(RwLock::new(())),
        })
    }

    pub async fn initialize(&self, repository: &Repository) -> Result<()> {
        tokio::fs::create_dir_all(&self.directory)
            .await
            .map_err(|error| cache_error(&self.directory, "create", error))?;
        let removed = self.prune(repository).await?;
        if removed > 0 {
            tracing::info!(removed, "pruned orphaned cover cache files at startup");
        }
        Ok(())
    }

    pub async fn get(&self, subject_id: i64) -> std::result::Result<CoverAsset, String> {
        let _guard = self.filesystem_guard.read().await;
        let cached = self.read_cached(subject_id).await;
        if let Some((asset, true)) = cached {
            return Ok(asset);
        }

        match self.download(subject_id).await {
            Ok(asset) => Ok(asset),
            Err(error) => {
                if let Some((asset, _)) = self.read_cached(subject_id).await {
                    tracing::warn!(subject_id, %error, "cover refresh failed; serving stale cache");
                    Ok(asset)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub async fn prune(&self, repository: &Repository) -> Result<usize> {
        let subjects = repository
            .list_anime()
            .await?
            .into_iter()
            .filter_map(|anime| anime.bangumi_subject_id)
            .filter(|subject_id| *subject_id > 0)
            .collect::<HashSet<_>>();
        self.prune_to_subjects(&subjects).await
    }

    async fn read_cached(&self, subject_id: i64) -> Option<(CoverAsset, bool)> {
        let path = self.cache_path(subject_id);
        let metadata = tokio::fs::metadata(&path).await.ok()?;
        let fresh = metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age <= SERVER_CACHE_TTL);
        let bytes = tokio::fs::read(&path).await.ok()?;
        match asset_from_bytes(bytes) {
            Some(asset) => Some((asset, fresh)),
            None => {
                if let Err(error) = tokio::fs::remove_file(&path).await
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(path = %path.display(), %error, "failed to remove invalid cover cache file");
                }
                None
            }
        }
    }

    async fn download(&self, subject_id: i64) -> std::result::Result<CoverAsset, String> {
        let api_url = format!(
            "{}/v0/subjects/{subject_id}/image?type=medium",
            self.api_base_url
        );
        let primary = self.download_image_url(api_url, self.request_timeout);
        let fallback = async {
            tokio::time::sleep(FALLBACK_DELAY).await;
            let cover_url = self.discover_cover_url(subject_id).await?;
            self.download_image_url(cover_url, self.fallback_request_timeout)
                .await
        };
        tokio::pin!(primary);
        tokio::pin!(fallback);
        let asset = tokio::select! {
            result = &mut primary => match result {
                Ok(asset) => asset,
                Err(primary_error) => fallback.await.map_err(|fallback_error| {
                    format!("{primary_error}; official subject page fallback failed: {fallback_error}")
                })?,
            },
            result = &mut fallback => match result {
                Ok(asset) => asset,
                Err(fallback_error) => primary.await.map_err(|primary_error| {
                    format!("{primary_error}; official subject page fallback failed: {fallback_error}")
                })?,
            },
        };
        self.store(subject_id, &asset.bytes).await?;
        Ok(asset)
    }

    async fn download_image_url(
        &self,
        url: String,
        timeout: Duration,
    ) -> std::result::Result<CoverAsset, String> {
        let mut response = self
            .client
            .get(&url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| format!("Bangumi cover request failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Bangumi cover request returned HTTP {}",
                response.status()
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_COVER_BYTES as u64)
        {
            return Err("Bangumi cover exceeds the 5 MiB limit".into());
        }

        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("Bangumi cover download failed: {error}"))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_COVER_BYTES {
                return Err("Bangumi cover exceeds the 5 MiB limit".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let asset = asset_from_bytes(bytes)
            .ok_or_else(|| "Bangumi returned an unsupported image format".to_string())?;
        Ok(asset)
    }

    async fn discover_cover_url(&self, subject_id: i64) -> std::result::Result<String, String> {
        let mut response = self
            .client
            .get(format!("https://bgm.tv/subject/{subject_id}"))
            .timeout(self.fallback_request_timeout)
            .send()
            .await
            .map_err(|error| format!("Bangumi subject page request failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Bangumi subject page returned HTTP {}",
                response.status()
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("Bangumi subject page download failed: {error}"))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_SUBJECT_PAGE_BYTES {
                return Err("Bangumi subject page exceeds the 2 MiB limit".into());
            }
            body.extend_from_slice(&chunk);
        }
        let html = String::from_utf8_lossy(&body);
        extract_official_cover_url(&html)
            .ok_or_else(|| "Bangumi subject page contains no official cover URL".into())
    }

    async fn store(&self, subject_id: i64, bytes: &[u8]) -> std::result::Result<(), String> {
        tokio::fs::create_dir_all(&self.directory)
            .await
            .map_err(|error| format!("cannot create cover cache directory: {error}"))?;
        let target = self.cache_path(subject_id);
        let temporary = self
            .directory
            .join(format!(".{subject_id}.{}.tmp", Uuid::new_v4().simple()));
        tokio::fs::write(&temporary, bytes)
            .await
            .map_err(|error| format!("cannot write cover cache: {error}"))?;
        if let Err(error) = tokio::fs::rename(&temporary, &target).await {
            let replace_existing = error.kind() == std::io::ErrorKind::AlreadyExists
                || (cfg!(windows) && target.exists());
            if replace_existing {
                tokio::fs::remove_file(&target)
                    .await
                    .map_err(|error| format!("cannot replace cover cache: {error}"))?;
                tokio::fs::rename(&temporary, &target)
                    .await
                    .map_err(|error| format!("cannot replace cover cache: {error}"))?;
            } else {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(format!("cannot install cover cache: {error}"));
            }
        }
        Ok(())
    }

    async fn prune_to_subjects(&self, subjects: &HashSet<i64>) -> Result<usize> {
        let _guard = self.filesystem_guard.write().await;
        tokio::fs::create_dir_all(&self.directory)
            .await
            .map_err(|error| cache_error(&self.directory, "create", error))?;
        let mut entries = tokio::fs::read_dir(&self.directory)
            .await
            .map_err(|error| cache_error(&self.directory, "read", error))?;
        let mut removed = 0;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| cache_error(&self.directory, "read", error))?
        {
            let path = entry.path();
            if !entry
                .file_type()
                .await
                .map_err(|error| cache_error(&path, "inspect", error))?
                .is_file()
            {
                continue;
            }
            let extension = path.extension().and_then(|value| value.to_str());
            let remove = match extension {
                Some("tmp") => true,
                Some("image") => path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .and_then(|value| value.parse::<i64>().ok())
                    .is_none_or(|subject_id| !subjects.contains(&subject_id)),
                _ => false,
            };
            if remove {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|error| cache_error(&path, "remove", error))?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn cache_path(&self, subject_id: i64) -> PathBuf {
        self.directory.join(format!("{subject_id}.image"))
    }
}

fn asset_from_bytes(bytes: Vec<u8>) -> Option<CoverAsset> {
    let content_type = image_content_type(&bytes)?;
    let etag = format!("\"{:x}\"", md5::compute(&bytes));
    Some(CoverAsset {
        bytes,
        content_type,
        etag,
    })
}

fn image_content_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(&bytes[8..12], b"avif" | b"avis")
    {
        Some("image/avif")
    } else {
        None
    }
}

fn extract_official_cover_url(html: &str) -> Option<String> {
    for prefix in ["https://lain.bgm.tv/", "//lain.bgm.tv/"] {
        for (start, _) in html.match_indices(prefix) {
            let tail = &html[start..];
            let end = tail
                .find(|character: char| {
                    matches!(
                        character,
                        '\"' | '\'' | '<' | '>' | ' ' | '\t' | '\r' | '\n'
                    )
                })
                .unwrap_or(tail.len());
            let candidate = &tail[..end];
            let candidate = if candidate.starts_with("//") {
                format!("https:{candidate}")
            } else {
                candidate.to_string()
            };
            let Ok(url) = url::Url::parse(&candidate) else {
                continue;
            };
            if url.scheme() == "https"
                && url.host_str() == Some("lain.bgm.tv")
                && url.path().contains("/pic/cover/")
            {
                return Some(candidate);
            }
        }
    }
    None
}

fn cache_error(path: &Path, operation: &str, error: std::io::Error) -> AppError {
    AppError::Config(format!(
        "cannot {operation} cover cache path {}: {error}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, routing::get};

    use super::*;

    #[test]
    fn only_recognized_raster_images_are_served() {
        assert_eq!(
            image_content_type(&[0xff, 0xd8, 0xff, 0x00]),
            Some("image/jpeg")
        );
        assert_eq!(
            image_content_type(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(image_content_type(b"not an image"), None);
    }

    #[test]
    fn extracts_only_official_bangumi_cover_urls() {
        assert_eq!(
            extract_official_cover_url(
                r#"<a href="//lain.bgm.tv/pic/cover/l/14/a1/622206_pNnzQ.jpg">cover</a>"#
            )
            .as_deref(),
            Some("https://lain.bgm.tv/pic/cover/l/14/a1/622206_pNnzQ.jpg")
        );
        assert!(
            extract_official_cover_url(
                r#"<img src="https://evil.example/pic/cover/l/not-bangumi.jpg">"#
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn pruning_removes_orphans_and_temporary_files_only() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut config = AppConfig::default();
        config.web.cover_cache_dir = directory.path().display().to_string();
        let cache = CoverCache::new(&config).unwrap();
        tokio::fs::write(cache.cache_path(1), [0xff, 0xd8, 0xff])
            .await
            .unwrap();
        tokio::fs::write(cache.cache_path(2), [0xff, 0xd8, 0xff])
            .await
            .unwrap();
        tokio::fs::write(directory.path().join(".2.download.tmp"), b"partial")
            .await
            .unwrap();
        tokio::fs::write(directory.path().join("keep.txt"), b"owned by operator")
            .await
            .unwrap();

        let removed = cache.prune_to_subjects(&HashSet::from([1])).await.unwrap();

        assert_eq!(removed, 2);
        assert!(cache.cache_path(1).exists());
        assert!(!cache.cache_path(2).exists());
        assert!(directory.path().join("keep.txt").exists());
    }

    #[tokio::test]
    async fn first_read_downloads_and_second_read_uses_disk_cache() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_counter = requests.clone();
        let app = Router::new().route(
            "/v0/subjects/{id}/image",
            get(move || {
                let request_counter = request_counter.clone();
                async move {
                    request_counter.fetch_add(1, Ordering::SeqCst);
                    (
                        [("content-type", "image/jpeg")],
                        vec![0xff, 0xd8, 0xff, 0xd9],
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let directory = tempfile::TempDir::new().unwrap();
        let mut config = AppConfig::default();
        config.web.cover_cache_dir = directory.path().display().to_string();
        config.schedule.bangumi_api_base_url = format!("http://{address}");
        let cache = CoverCache::new(&config).unwrap();

        assert_eq!(cache.get(42).await.unwrap().content_type, "image/jpeg");
        assert_eq!(cache.get(42).await.unwrap().content_type, "image/jpeg");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(cache.cache_path(42).exists());

        server.abort();
    }
}
