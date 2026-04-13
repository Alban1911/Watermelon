use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};

use super::discovery::LcuInfo;

/// HTTPS client for the LCU API. Accepts the LCU's self-signed localhost cert.
pub struct LcuClient {
    http: Client,
    info: LcuInfo,
}

impl LcuClient {
    pub fn new(info: LcuInfo) -> Result<Self> {
        let http = Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, info })
    }

    pub async fn get(&self, path: &str) -> Result<String> {
        let url = format!("https://127.0.0.1:{}{}", self.info.port, path);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", &self.info.auth_header)
            .header("Accept", "application/json")
            .send()
            .await
            .context("sending request")?;

        resp.text().await.context("reading body")
    }

    pub async fn post_empty(&self, path: &str) -> Result<StatusCode> {
        let url = format!("https://127.0.0.1:{}{}", self.info.port, path);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", &self.info.auth_header)
            .header("Accept", "application/json")
            .send()
            .await
            .context("sending request")?;

        Ok(resp.status())
    }
}
