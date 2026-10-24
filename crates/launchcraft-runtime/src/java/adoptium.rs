use reqwest::Client;
use serde::Deserialize;
use crate::error::RuntimeError;
use crate::platform::PlatformInfo;

#[derive(Debug, Deserialize)]
pub struct AdoptiumBinary {
    pub package: Option<AdoptiumPackage>,
}

#[derive(Debug, Deserialize)]
pub struct AdoptiumPackage {
    pub link: String,
    pub checksum: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct AdoptiumRelease {
    pub release_name: String,
    pub binaries: Vec<AdoptiumBinary>,
}

pub struct AdoptiumClient {
    client: Client,
    base_url: String,
}

impl Default for AdoptiumClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AdoptiumClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder().build().unwrap_or_default(),
            base_url: "https://api.adoptium.net/v3".to_string(),
        }
    }

    pub fn build_download_url(&self, major_version: u32, platform: &PlatformInfo) -> String {
        format!(
            "{}/binary/latest/{}/ga/{}/{}/jdk/hotspot/normal/eclipse",
            self.base_url,
            major_version,
            platform.adoptium_os_name(),
            platform.adoptium_arch_name()
        )
    }

    pub async fn fetch_latest_release(
        &self,
        major_version: u32,
        platform: &PlatformInfo,
    ) -> Result<AdoptiumRelease, RuntimeError> {
        let url = format!(
            "{}/assets/latest/{}/hotspot?os={}&architecture={}&image_type=jdk",
            self.base_url,
            major_version,
            platform.adoptium_os_name(),
            platform.adoptium_arch_name()
        );

        let res = self.client.get(&url).send().await?;
        let releases: Vec<AdoptiumRelease> = res.json().await?;
        releases
            .into_iter()
            .next()
            .ok_or_else(|| RuntimeError::JavaNotFound(format!("No Adoptium release found for Java {}", major_version)))
    }
}
