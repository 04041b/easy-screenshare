use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateSessionResponse {
    pub id: String,
    pub sender_token: String,
    pub pin: String,
    pub viewer_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SdpEnvelope {
    pub sdp: String,
}

#[derive(Debug, Deserialize)]
pub struct FallbackFlag {
    pub fallback: bool,
}

pub struct SignalingClient {
    base: String,
    http: reqwest::Client,
}

impl SignalingClient {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
        }
    }

    pub async fn create_session(&self) -> anyhow::Result<CreateSessionResponse> {
        let r = self
            .http
            .post(format!("{}/api/sessions", self.base))
            .send()
            .await?
            .error_for_status()?;
        Ok(r.json().await?)
    }

    pub async fn put_offer(&self, id: &str, token: &str, sdp: &str) -> anyhow::Result<()> {
        self.http
            .put(format!("{}/api/sessions/{}/offer", self.base, id))
            .header("X-Sender-Token", token)
            .json(&SdpEnvelope { sdp: sdp.into() })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn get_offer(&self, id: &str, pin: &str) -> anyhow::Result<Option<SdpEnvelope>> {
        let r = self
            .http
            .get(format!("{}/api/sessions/{}/offer", self.base, id))
            .header("X-Viewer-Pin", pin)
            .send()
            .await?;
        if r.status() == 404 {
            return Ok(None);
        }
        if r.status() == 401 {
            anyhow::bail!("invalid PIN");
        }
        if r.status() == 423 {
            anyhow::bail!("session locked (too many failed PIN attempts)");
        }
        Ok(Some(r.error_for_status()?.json().await?))
    }

    pub async fn put_answer(&self, id: &str, pin: &str, sdp: &str) -> anyhow::Result<()> {
        self.http
            .put(format!("{}/api/sessions/{}/answer", self.base, id))
            .header("X-Viewer-Pin", pin)
            .json(&SdpEnvelope { sdp: sdp.into() })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn get_answer(&self, id: &str, token: &str) -> anyhow::Result<Option<SdpEnvelope>> {
        let r = self
            .http
            .get(format!("{}/api/sessions/{}/answer", self.base, id))
            .header("X-Sender-Token", token)
            .send()
            .await?;
        if r.status() == 404 {
            return Ok(None);
        }
        Ok(Some(r.error_for_status()?.json().await?))
    }

    pub async fn put_fallback(&self, id: &str, token: &str) -> anyhow::Result<()> {
        self.http
            .put(format!("{}/api/sessions/{}/fallback", self.base, id))
            .header("X-Sender-Token", token)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn get_fallback(&self, id: &str) -> anyhow::Result<bool> {
        let f: FallbackFlag = self
            .http
            .get(format!("{}/api/sessions/{}/fallback", self.base, id))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(f.fallback)
    }
}

/// Build the WS relay URL.
/// - sender supplies `token = Some(sender_token)`, `pin = None`
/// - viewer supplies `token = None`, `pin = Some(pin)`
pub fn ws_url(base_http: &str, id: &str, role: &str, token: Option<&str>, pin: Option<&str>) -> String {
    let scheme = if base_http.starts_with("https://") { "wss://" } else { "ws://" };
    let host = base_http
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let mut url = format!("{scheme}{host}/ws/relay/{id}?role={role}");
    if let Some(t) = token { url.push_str(&format!("&token={t}")); }
    if let Some(p) = pin { url.push_str(&format!("&pin={p}")); }
    url
}
