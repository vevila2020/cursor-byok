//! Downloads and verifies independently released first-party plugins.
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{network, store::Store, Error, Result};

const RELEASES_URL: &str =
    "https://api.github.com/repos/vevila2020/cursor-byok/releases?per_page=100";
const PUBLIC_KEY: &str = "RWSahkj9aRtUF3bN4xEtFWFHu2By2Z687SZ9e4nVlaWCSu2MWS9ehSmo";
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ARCHIVE_FILES: usize = 256;
const RELEASE_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdateInfo {
    pub plugin_id: String,
    pub installed_version: String,
    pub available_version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdateStage {
    pub plugin_id: String,
    pub version: String,
}

type ReleaseCache = Option<(tokio::time::Instant, Vec<PluginRelease>)>;

#[derive(Clone)]
pub(super) struct PluginUpdater {
    store: Store,
    installed_root: PathBuf,
    app_version: String,
    releases: Arc<Mutex<ReleaseCache>>,
    install_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug)]
struct PluginRelease {
    plugin_id: String,
    version: String,
    archive_url: String,
    signature_url: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

impl PluginUpdater {
    pub fn managed(store: Store, app_version: String) -> Result<Self> {
        Ok(Self {
            store,
            installed_root: crate::config::managed_data_dir()?.join("plugins/installed"),
            app_version,
            releases: Arc::new(Mutex::new(None)),
            install_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn available(&self, force_refresh: bool) -> Result<Vec<PluginUpdateInfo>> {
        let releases = self.releases(force_refresh).await?;
        let mut updates = Vec::new();
        for (plugin_id, slug) in managed_plugins() {
            let Some(installed_version) = installed_version(&self.installed_root, slug) else {
                continue;
            };
            if let Some(release) = releases
                .iter()
                .find(|release| release.plugin_id == plugin_id)
            {
                if version_is_newer(&release.version, &installed_version) {
                    updates.push(PluginUpdateInfo {
                        plugin_id: plugin_id.to_owned(),
                        installed_version,
                        available_version: release.version.clone(),
                    });
                }
            }
        }
        Ok(updates)
    }

    pub async fn install(
        &self,
        plugin_id: &str,
        executable: &Path,
        loader: &super::definition::PluginDefinitionLoader,
    ) -> Result<PluginUpdateStage> {
        let _guard = self.install_lock.lock().await;
        let slug = plugin_slug(plugin_id)
            .ok_or_else(|| Error::RunNotFound(format!("managed plugin {plugin_id}")))?;
        let current = installed_version(&self.installed_root, slug)
            .ok_or_else(|| Error::RunNotFound(format!("installed plugin {plugin_id}")))?;
        let release = self
            .releases(false)
            .await?
            .into_iter()
            .find(|release| release.plugin_id == plugin_id)
            .filter(|release| version_is_newer(&release.version, &current))
            .ok_or_else(|| Error::RunNotFound(format!("update for plugin {plugin_id}")))?;

        let client = network::client(&self.store).await?;
        let (archive, signature) = tokio::try_join!(
            download_bytes(&client, &release.archive_url),
            download_text(&client, &release.signature_url),
        )?;
        verify_archive(&archive, &signature)?;

        let target = self.installed_root.join(slug);
        let parent = target
            .parent()
            .ok_or_else(|| Error::Config("plugin install directory has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let suffix = uuid::Uuid::new_v4();
        let staged = parent.join(format!(".{slug}.stage-{suffix}"));
        let backup = parent.join(format!(".{slug}.backup-{suffix}"));
        if let Err(error) = extract_archive(&archive, &staged) {
            let _ = fs::remove_dir_all(&staged);
            return Err(error);
        }
        let validation = async {
            verify_staged_plugin(&staged, plugin_id, &release.version, &self.app_version)?;
            let manifest_bytes = tokio::fs::read(staged.join("plugin.json")).await?;
            let manifest: super::manifest::PluginManifest =
                serde_json::from_slice(&manifest_bytes)?;
            let entry = staged.join(&manifest.entry).canonicalize()?;
            let definition = loader.load(executable, &staged, &entry).await?;
            super::catalog::validate_definition(plugin_id, &definition)
        }
        .await;
        if let Err(error) = validation {
            let _ = fs::remove_dir_all(&staged);
            return Err(error);
        }

        fs::rename(&target, &backup)?;
        if let Err(error) = fs::rename(&staged, &target) {
            let _ = fs::rename(&backup, &target);
            let _ = fs::remove_dir_all(&staged);
            return Err(error.into());
        }
        if let Err(error) = fs::remove_dir_all(backup) {
            tracing::warn!(plugin = %plugin_id, %error, "failed to remove previous plugin files");
        }
        Ok(PluginUpdateStage {
            plugin_id: plugin_id.to_owned(),
            version: release.version,
        })
    }

    async fn releases(&self, force_refresh: bool) -> Result<Vec<PluginRelease>> {
        let mut cache = self.releases.lock().await;
        if !force_refresh {
            if let Some((checked_at, releases)) = cache.as_ref() {
                if checked_at.elapsed() < RELEASE_CACHE_TTL {
                    return Ok(releases.clone());
                }
            }
        }
        let client = network::client(&self.store).await?;
        let response = client
            .get(RELEASES_URL)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "Cursor-BYOK-Plugin-Updater")
            .timeout(Duration::from_secs(20))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::Provider(format!(
                "plugin update check failed (HTTP {})",
                response.status()
            )));
        }
        let release_list: Vec<GitHubRelease> = response.json().await?;
        let mut latest: HashMap<String, PluginRelease> = HashMap::new();
        for release in release_list {
            if release.draft || !release.prerelease {
                continue;
            }
            let Some((plugin_id, version, slug)) = parse_plugin_tag(&release.tag_name) else {
                continue;
            };
            let archive_name = format!("{slug}-{version}.zip");
            let signature_name = format!("{archive_name}.sig");
            let Some(archive) = release
                .assets
                .iter()
                .find(|asset| asset.name == archive_name)
            else {
                continue;
            };
            let Some(signature) = release
                .assets
                .iter()
                .find(|asset| asset.name == signature_name)
            else {
                continue;
            };
            let candidate = PluginRelease {
                plugin_id: plugin_id.to_owned(),
                version: version.to_owned(),
                archive_url: archive.browser_download_url.clone(),
                signature_url: signature.browser_download_url.clone(),
            };
            match latest.get(plugin_id) {
                Some(existing) if !version_is_newer(version, &existing.version) => {}
                _ => {
                    latest.insert(plugin_id.to_owned(), candidate);
                }
            }
        }
        let releases: Vec<_> = latest.into_values().collect();
        *cache = Some((tokio::time::Instant::now(), releases.clone()));
        Ok(releases)
    }
}

fn managed_plugins() -> [(&'static str, &'static str); 3] {
    [
        ("dev.cursorbyok.examples.codex-auth", "codex-auth"),
        ("dev.cursorbyok.examples.grok-auth", "grok-auth"),
        (
            "dev.cursorbyok.plugins.antigravity-auth",
            "antigravity-auth",
        ),
    ]
}

fn plugin_slug(plugin_id: &str) -> Option<&'static str> {
    managed_plugins()
        .into_iter()
        .find_map(|(id, slug)| (id == plugin_id).then_some(slug))
}

fn parse_plugin_tag(tag: &str) -> Option<(&'static str, &str, &'static str)> {
    for (plugin_id, slug) in managed_plugins() {
        let prefix = format!("plugin-{slug}-v");
        if let Some(version) = tag.strip_prefix(&prefix) {
            if crate::plugin::manifest::parse_version(version).is_some() {
                return Some((plugin_id, version, slug));
            }
        }
    }
    None
}

fn installed_version(root: &Path, slug: &str) -> Option<String> {
    let manifest = fs::read(root.join(slug).join("plugin.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&manifest).ok()?;
    value.get("version")?.as_str().map(str::to_owned)
}

fn version_is_newer(candidate: &str, installed: &str) -> bool {
    match (
        crate::plugin::manifest::parse_version(candidate),
        crate::plugin::manifest::parse_version(installed),
    ) {
        (Some(candidate), Some(installed)) => candidate > installed,
        _ => false,
    }
}

async fn download_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(60))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(Error::Provider(format!(
            "plugin package download failed (HTTP {})",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARCHIVE_BYTES as u64)
    {
        return Err(Error::Config(
            "plugin package exceeds the size limit".into(),
        ));
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(Error::Config(
            "plugin package exceeds the size limit".into(),
        ));
    }
    Ok(bytes.to_vec())
}

async fn download_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(Error::Provider(format!(
            "plugin signature download failed (HTTP {})",
            response.status()
        )));
    }
    Ok(response.text().await?)
}

fn verify_archive(archive: &[u8], signature: &str) -> Result<()> {
    let key = PublicKey::from_base64(PUBLIC_KEY)
        .map_err(|error| Error::Config(format!("invalid embedded plugin signing key: {error}")))?;
    let signature = Signature::decode(signature)
        .map_err(|error| Error::Config(format!("invalid plugin package signature: {error}")))?;
    key.verify(archive, &signature, false).map_err(|error| {
        Error::Config(format!(
            "plugin package signature verification failed: {error}"
        ))
    })
}

fn extract_archive(archive: &[u8], destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    let result = (|| -> Result<()> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
            .map_err(|error| Error::Config(format!("invalid plugin package: {error}")))?;
        if zip.len() > MAX_ARCHIVE_FILES {
            return Err(Error::Config(
                "plugin package contains too many files".into(),
            ));
        }
        let mut total_bytes = 0_u64;
        for index in 0..zip.len() {
            let mut file = zip
                .by_index(index)
                .map_err(|error| Error::Config(format!("invalid plugin package: {error}")))?;
            let path = file
                .enclosed_name()
                .ok_or_else(|| Error::Config("plugin package contains an unsafe path".into()))?
                .to_path_buf();
            if file
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                return Err(Error::Config(
                    "plugin package cannot contain symlinks".into(),
                ));
            }
            total_bytes = total_bytes.saturating_add(file.size());
            if total_bytes > MAX_EXTRACTED_BYTES {
                return Err(Error::Config(
                    "plugin package expands beyond the size limit".into(),
                ));
            }
            let output = destination.join(path);
            if file.is_dir() {
                fs::create_dir_all(output)?;
                continue;
            }
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output_file = fs::File::create(output)?;
            let copied = std::io::copy(&mut file, &mut output_file)?;
            if copied != file.size() {
                return Err(Error::Config("plugin package entry was truncated".into()));
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

fn verify_staged_plugin(
    directory: &Path,
    plugin_id: &str,
    version: &str,
    app_version: &str,
) -> Result<()> {
    let bytes = fs::read(directory.join("plugin.json"))?;
    let manifest: super::manifest::PluginManifest = serde_json::from_slice(&bytes)?;
    manifest.validate(directory)?;
    super::catalog::require_app_version(&manifest, app_version)?;
    if manifest.id != plugin_id || manifest.version != version {
        return Err(Error::Config(
            "plugin package identity or version does not match its release".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            writer.start_file(*name, options).unwrap();
            std::io::Write::write_all(&mut writer, content).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn recognizes_only_versioned_managed_plugin_tags() {
        assert_eq!(
            parse_plugin_tag("plugin-codex-auth-v0.2.0"),
            Some(("dev.cursorbyok.examples.codex-auth", "0.2.0", "codex-auth"))
        );
        assert!(parse_plugin_tag("plugin-other-v0.2.0").is_none());
        assert!(parse_plugin_tag("plugin-grok-auth-vlatest").is_none());
    }

    #[test]
    fn compares_supported_plugin_versions() {
        assert!(version_is_newer("0.2.0", "0.1.9"));
        assert!(!version_is_newer("0.1.9", "0.1.9"));
        assert!(!version_is_newer("0.1.8", "0.1.9"));
    }

    #[test]
    fn verifies_update_archives_with_the_desktop_signing_key() {
        let signature = "untrusted comment: signature from minisign secret key\nRUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\ntrusted comment: timestamp:1633700835\tfile:test\tprehashed\nwLMDjy9FLAuxZ3q4NlEvkgtyhrr0gtTu6KC4KBJdITbbOeAi1zBIYo0v4iTgt8jJpIidRJnp94ABQkJAgAooBQ==";
        let key = PublicKey::from_base64(PUBLIC_KEY).unwrap();
        let signature = Signature::decode(signature).unwrap();
        assert!(key.verify(b"test", &signature, false).is_err());
        let test_key =
            PublicKey::from_base64("RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3")
                .unwrap();
        test_key.verify(b"test", &signature, false).unwrap();
        assert!(key.verify(b"tampered", &signature, false).is_err());
    }

    #[test]
    fn rejects_archive_paths_that_escape_the_staging_directory() {
        let archive = archive_with(&[("../outside.txt", b"unsafe")]);
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("staged");
        assert!(extract_archive(&archive, &destination).is_err());
        assert!(!destination.exists());
        assert!(!root.path().join("outside.txt").exists());
    }

    #[test]
    fn validates_staged_plugin_identity_and_version_before_install() {
        let archive = archive_with(&[
            (
                "plugin.json",
                br#"{"apiVersion":1,"id":"dev.cursorbyok.examples.codex-auth","name":"Codex","version":"0.2.0","minAppVersion":"0.1.0","icon":"assets/plugin.svg","entry":"main.ts","permissions":{"network":[]}}"#,
            ),
            ("main.ts", b"export {};"),
            ("assets/plugin.svg", b"<svg/>")
        ]);
        let root = tempfile::tempdir().unwrap();
        let staged = root.path().join("staged");
        extract_archive(&archive, &staged).unwrap();
        verify_staged_plugin(
            &staged,
            "dev.cursorbyok.examples.codex-auth",
            "0.2.0",
            "0.1.0",
        )
        .unwrap();
        assert!(verify_staged_plugin(
            &staged,
            "dev.cursorbyok.examples.grok-auth",
            "0.2.0",
            "0.1.0",
        )
        .is_err());
    }
}
