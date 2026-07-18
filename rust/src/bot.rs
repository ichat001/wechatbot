//! Main WeChatBot client.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use crate::cdn::CdnClient;
use crate::crypto;
use crate::error::{Result, WeChatBotError};
use crate::protocol::{self, ILinkClient};
use crate::types::*;
use md5::{Digest, Md5};
use rand::Rng;
use serde_json::json;

/// Message handler callback type.
pub type MessageHandler = Box<dyn Fn(&IncomingMessage) + Send + Sync>;

/// Default pairing-code prompt: read a line from stdin without blocking the runtime.
async fn read_verify_code_from_stdin(is_retry: bool) -> Result<String> {
    tokio::task::spawn_blocking(move || -> Result<String> {
        use std::io::{BufRead, Write};
        let prompt = if is_retry {
            "Code mismatch — enter the pairing code shown in WeChat again: "
        } else {
            "Enter the pairing code shown in WeChat on your phone: "
        };
        eprint!("{}", prompt);
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim().to_string())
    })
    .await
    .map_err(|e| WeChatBotError::Auth(format!("pairing code prompt failed: {e}")))?
}

/// Bot configuration options.
pub struct BotOptions {
    pub base_url: Option<String>,
    pub cred_path: Option<String>,
    pub on_qr_url: Option<Box<dyn Fn(&str) + Send + Sync>>,
    pub on_error: Option<Box<dyn Fn(&WeChatBotError) + Send + Sync>>,
    /// UA-style identifier of the app driving this bot, sent as
    /// `base_info.bot_agent` on every API request (e.g. "MyApp/1.2 (prod)").
    /// Invalid values fall back to `protocol::default_bot_agent()`.
    pub bot_agent: Option<String>,
    /// Called when the server requires a pairing code (the digits shown in
    /// WeChat on the user's phone). The argument is true when a previously
    /// submitted code was rejected. Defaults to a stdin prompt.
    pub on_verify_code: Option<Box<dyn Fn(bool) -> String + Send + Sync>>,
}

impl Default for BotOptions {
    fn default() -> Self {
        Self {
            base_url: None,
            cred_path: None,
            on_qr_url: None,
            on_error: None,
            bot_agent: None,
            on_verify_code: None,
        }
    }
}

/// WeChatBot is the main entry point.
pub struct WeChatBot {
    client: Arc<ILinkClient>,
    cdn: CdnClient,
    credentials: RwLock<Option<Credentials>>,
    context_tokens: RwLock<HashMap<String, String>>,
    handlers: Mutex<Vec<MessageHandler>>,
    cursor: RwLock<String>,
    base_url: RwLock<String>,
    cred_path: Option<String>,
    stopped: RwLock<bool>,
    on_qr_url: Option<Box<dyn Fn(&str) + Send + Sync>>,
    on_error: Option<Box<dyn Fn(&WeChatBotError) + Send + Sync>>,
    on_verify_code: Option<Box<dyn Fn(bool) -> String + Send + Sync>>,
}

impl WeChatBot {
    /// Create a new bot instance.
    pub fn new(opts: BotOptions) -> Self {
        Self {
            client: Arc::new(ILinkClient::with_bot_agent(opts.bot_agent.as_deref())),
            cdn: CdnClient::new(),
            credentials: RwLock::new(None),
            context_tokens: RwLock::new(HashMap::new()),
            handlers: Mutex::new(Vec::new()),
            cursor: RwLock::new(String::new()),
            base_url: RwLock::new(
                opts.base_url
                    .unwrap_or_else(|| protocol::DEFAULT_BASE_URL.to_string()),
            ),
            cred_path: opts.cred_path,
            stopped: RwLock::new(false),
            on_qr_url: opts.on_qr_url,
            on_error: opts.on_error,
            on_verify_code: opts.on_verify_code,
        }
    }

    // --- QR login flow (unchanged) ---
    const MAX_QR_REFRESH: u32 = 3;
    const FIXED_QR_BASE_URL: &'static str = "https://ilinkai.weixin.qq.com";

    pub async fn login(&self, force: bool) -> Result<Credentials> {
        let base_url = self.base_url.read().await.clone();

        let stored = self.load_credentials().await?;
        if !force {
            if let Some(creds) = stored.clone() {
                *self.credentials.write().await = Some(creds.clone());
                *self.base_url.write().await = creds.base_url.clone();
                info!("Loaded stored credentials for {}", creds.user_id);
                return Ok(creds);
            }
        }

        let local_token_list: Vec<String> = stored
            .as_ref()
            .filter(|c| !c.token.is_empty())
            .map(|c| vec![c.token.clone()])
            .unwrap_or_default();

        let mut qr_refresh_count = 0u32;
        loop {
            qr_refresh_count += 1;
            if qr_refresh_count > Self::MAX_QR_REFRESH {
                return Err(WeChatBotError::Auth(format!(
                    "QR code expired {} times — login aborted",
                    Self::MAX_QR_REFRESH
                )));
            }

            let qr = self
                .client
                .get_qr_code(Self::FIXED_QR_BASE_URL, &local_token_list)
                .await?;

            if let Some(ref cb) = self.on_qr_url {
                cb(&qr.qrcode_img_content);
            } else {
                eprintln!("[wechatbot] Scan: {}", qr.qrcode_img_content);
            }

            let mut last_status = String::new();
            let mut current_poll_base_url = Self::FIXED_QR_BASE_URL.to_string();
            let mut pending_verify_code: Option<String> = None;
            loop {
                let status = self
                    .client
                    .poll_qr_status(
                        &current_poll_base_url,
                        &qr.qrcode,
                        pending_verify_code.as_deref(),
                    )
                    .await?;

                if status.status != last_status {
                    last_status = status.status.clone();
                    match status.status.as_str() {
                        "scaned" => {
                            pending_verify_code = None;
                            info!("QR scanned — confirm in WeChat");
                        }
                        "expired" => warn!("QR expired — requesting new one"),
                        "confirmed" => info!("Login confirmed"),
                        _ => {}
                    }
                }

                if status.status == "need_verifycode" {
                    let is_retry = pending_verify_code.is_some();
                    let code = match self.on_verify_code {
                        Some(ref cb) => cb(is_retry),
                        None => read_verify_code_from_stdin(is_retry).await?,
                    };
                    pending_verify_code = Some(code);
                    continue;
                }

                if status.status == "verify_code_blocked" {
                    warn!("Pairing code blocked after repeated mismatches — requesting new QR");
                    break;
                }

                if status.status == "confirmed" {
                    let token = status
                        .bot_token
                        .ok_or_else(|| WeChatBotError::Auth("missing bot_token".into()))?;
                    let creds = Credentials {
                        token,
                        base_url: status.baseurl.unwrap_or_else(|| base_url.clone()),
                        account_id: status.ilink_bot_id.unwrap_or_default(),
                        user_id: status.ilink_user_id.unwrap_or_default(),
                        saved_at: Some(chrono_now()),
                    };
                    self.save_credentials(&creds).await?;
                    *self.credentials.write().await = Some(creds.clone());
                    *self.base_url.write().await = creds.base_url.clone();
                    return Ok(creds);
                }

                if status.status == "binded_redirect" {
                    if let Some(creds) = stored.clone() {
                        info!("Bot already bound — reusing stored credentials");
                        *self.credentials.write().await = Some(creds.clone());
                        *self.base_url.write().await = creds.base_url.clone();
                        return Ok(creds);
                    }
                    return Err(WeChatBotError::Auth(
                        "server reports this bot is already bound to this client \
                         (binded_redirect), but no local credentials were found"
                            .into(),
                    ));
                }

                if status.status == "scaned_but_redirect" {
                    if let Some(ref host) = status.redirect_host {
                        current_poll_base_url = format!("https://{}", host);
                        info!("IDC redirect, switching polling host to {}", host);
                    } else {
                        warn!("Received scaned_but_redirect but redirect_host is missing");
                    }
                    sleep(Duration::from_secs(2)).await;
                    continue;
                }

                if status.status == "expired" {
                    break;
                }

                sleep(Duration::from_secs(2)).await;
            }
        }
    }

    pub async fn on_message(&self, handler: MessageHandler) {
        self.handlers.lock().await.push(handler);
    }

    pub async fn reply(&self, msg: &IncomingMessage, text: &str) -> Result<()> {
        self.context_tokens
            .write()
            .await
            .insert(msg.user_id.clone(), msg.context_token.clone());
        self.send_text(&msg.user_id, text, &msg.context_token).await
    }

    pub async fn send(&self, user_id: &str, text: &str) -> Result<()> {
        let ct = self.context_tokens.read().await.get(user_id).cloned();
        let ct = ct.ok_or_else(|| WeChatBotError::NoContext(user_id.to_string()))?;
        self.send_text(user_id, text, &ct).await
    }

    pub async fn send_typing(&self, user_id: &str) -> Result<()> {
        let ct = self.context_tokens.read().await.get(user_id).cloned();
        let ct = ct.ok_or_else(|| WeChatBotError::NoContext(user_id.to_string()))?;
        let (base_url, token) = self.get_auth().await?;
        let config = self
            .client
            .get_config(&base_url, &token, user_id, &ct)
            .await?;
        if let Some(ticket) = config.typing_ticket {
            self.client
                .send_typing(&base_url, &token, user_id, &ticket, 1)
                .await?;
        }
        Ok(())
    }

    pub async fn reply_media(&self, msg: &IncomingMessage, content: SendContent) -> Result<()> {
        self.context_tokens
            .write()
            .await
            .insert(msg.user_id.clone(), msg.context_token.clone());
        self.send_content(&msg.user_id, &msg.context_token, content)
            .await
    }

    pub async fn send_media(&self, user_id: &str, content: SendContent) -> Result<()> {
        let ct = self.context_tokens.read().await.get(user_id).cloned();
        let ct = ct.ok_or_else(|| WeChatBotError::NoContext(user_id.to_string()))?;
        self.send_content(user_id, &ct, content).await
    }

    pub async fn download(&self, msg: &IncomingMessage) -> Result<Option<DownloadedMedia>> {
        if let Some(img) = msg.images.first() {
            if let Some(ref media) = img.media {
                let data = self.cdn.download(media, img.aes_key.as_deref()).await?;
                return Ok(Some(DownloadedMedia {
                    data,
                    media_type: "image".into(),
                    file_name: None,
                    format: None,
                }));
            }
        }
        if let Some(file) = msg.files.first() {
            if let Some(ref media) = file.media {
                let data = self.cdn.download(media, None).await?;
                return Ok(Some(DownloadedMedia {
                    data,
                    media_type: "file".into(),
                    file_name: Some(file.file_name.clone().unwrap_or_else(|| "file.bin".into())),
                    format: None,
                }));
            }
        }
        if let Some(video) = msg.videos.first() {
            if let Some(ref media) = video.media {
                let data = self.cdn.download(media, None).await?;
                return Ok(Some(DownloadedMedia {
                    data,
                    media_type: "video".into(),
                    file_name: None,
                    format: None,
                }));
            }
        }
        if let Some(voice) = msg.voices.first() {
            if let Some(ref media) = voice.media {
                let data = self.cdn.download(media, None).await?;
                return Ok(Some(DownloadedMedia {
                    data,
                    media_type: "voice".into(),
                    file_name: None,
                    format: Some("silk".into()),
                }));
            }
        }
        Ok(None)
    }

    pub async fn download_raw(
        &self,
        media: &CDNMedia,
        aeskey_override: Option<&str>,
    ) -> Result<Vec<u8>> {
        self.cdn.download(media, aeskey_override).await
    }

    pub async fn upload(
        &self,
        data: &[u8],
        user_id: &str,
        media_type: i32,
    ) -> Result<UploadResult> {
        let (base_url, token) = self.get_auth().await?;
        self.cdn_upload(&base_url, &token, data, user_id, media_type)
            .await
    }

    pub async fn run(&self) -> Result<()> {
        *self.stopped.write().await = false;

        {
            let (base_url, token) = self.get_auth().await?;
            if let Err(e) = self.client.notify_start(&base_url, &token).await {
                warn!("notify_start failed (ignored): {}", e);
            }
        }

        info!("Long-poll loop started");
        let mut retry_delay = Duration::from_secs(1);

        loop {
            if *self.stopped.read().await {
                break;
            }

            let (base_url, token) = self.get_auth().await?;
            let cursor = self.cursor.read().await.clone();

            match self.client.get_updates(&base_url, &token, &cursor).await {
                Ok(updates) => {
                    if !updates.get_updates_buf.is_empty() {
                        *self.cursor.write().await = updates.get_updates_buf;
                    }
                    retry_delay = Duration::from_secs(1);

                    for wire in &updates.msgs {
                        self.remember_context(wire).await;
                        if let Some(incoming) = IncomingMessage::from_wire(wire) {
                            let handlers = self.handlers.lock().await;
                            for handler in handlers.iter() {
                                handler(&incoming);
                            }
                        }
                    }
                }
                Err(e) if e.is_session_expired() => {
                    warn!("Session expired — re-login required");
                    *self.context_tokens.write().await = HashMap::new();
                    *self.cursor.write().await = String::new();
                    if let Err(e) = self.login(true).await {
                        self.report_error(&e);
                    }
                    continue;
                }
                Err(e) => {
                    self.report_error(&e);
                    sleep(retry_delay).await;
                    retry_delay = std::cmp::min(retry_delay * 2, Duration::from_secs(10));
                    continue;
                }
            }
        }

        if let Ok((base_url, token)) = self.get_auth().await {
            if let Err(e) = self.client.notify_stop(&base_url, &token).await {
                warn!("notify_stop failed (ignored): {}", e);
            }
        }

        info!("Long-poll loop stopped");
        Ok(())
    }

    pub async fn stop(&self) {
        *self.stopped.write().await = true;
    }

    // --- internal media ---

    fn send_content<'a>(
        &'a self,
        user_id: &'a str,
        context_token: &'a str,
        content: SendContent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let (base_url, token) = self.get_auth().await?;
            match content {
                SendContent::Text(text) => self.send_text(user_id, &text, context_token).await,
                SendContent::Image { data, caption } => {
                    let result = self
                        .cdn_upload(&base_url, &token, &data, user_id, 1)
                        .await?;
                    let mut items = Vec::new();
                    if let Some(cap) = caption {
                        items.push(json!({"type": 1, "text_item": {"text": cap}}));
                    }
                    items.push(json!({"type": 2, "image_item": {
                        "media": cdn_media_json(&result.media),
                        "mid_size": result.encrypted_file_size,
                    }}));
                    let msg = protocol::build_media_message(user_id, context_token, items);
                    // ---- 日志：发送消息 ----
                    let log_path = get_desktop_log_path("rust.log");
                    let _ = append_log(&log_path, &format!(
                        "sendmessage payload (Image): {}",
                        serde_json::to_string_pretty(&msg).unwrap()
                    ));
                    self.client.send_message(&base_url, &token, &msg).await
                }
                SendContent::Video { data, caption } => {
                    let result = self
                        .cdn_upload(&base_url, &token, &data, user_id, 2)
                        .await?;
                    let mut items = Vec::new();
                    if let Some(cap) = caption {
                        items.push(json!({"type": 1, "text_item": {"text": cap}}));
                    }
                    items.push(json!({"type": 5, "video_item": {
                        "media": cdn_media_json(&result.media),
                        "video_size": result.encrypted_file_size,
                    }}));
                    let msg = protocol::build_media_message(user_id, context_token, items);
                    // ---- 日志：发送消息 ----
                    let log_path = get_desktop_log_path("rust.log");
                    let _ = append_log(&log_path, &format!(
                        "sendmessage payload (Video): {}",
                        serde_json::to_string_pretty(&msg).unwrap()
                    ));
                    self.client.send_message(&base_url, &token, &msg).await
                }
                SendContent::File {
                    data,
                    file_name,
                    caption,
                } => {
                    let cat = categorize_by_extension(&file_name);
                    match cat {
                        "image" => {
                            self.send_content(
                                user_id,
                                context_token,
                                SendContent::Image { data, caption },
                            )
                            .await
                        }
                        "video" => {
                            self.send_content(
                                user_id,
                                context_token,
                                SendContent::Video { data, caption },
                            )
                            .await
                        }
                        _ => {
                            if let Some(cap) = caption {
                                self.send_text(user_id, &cap, context_token).await?;
                            }
                            let data_len = data.len();
                            let result = self
                                .cdn_upload(&base_url, &token, &data, user_id, 3)
                                .await?;
                            let items = vec![json!({"type": 4, "file_item": {
                                "media": cdn_media_json(&result.media),
                                "file_name": file_name,
                                "len": data_len.to_string(),
                            }})];
                            let msg = protocol::build_media_message(user_id, context_token, items);
                            self.client.send_message(&base_url, &token, &msg).await
                        }
                    }
                }
            }
        })
    }

    // --- 修改后的 cdn_upload（含详细日志）---
    async fn cdn_upload(
        &self,
        base_url: &str,
        token: &str,
        data: &[u8],
        user_id: &str,
        media_type: i32,
    ) -> Result<UploadResult> {
        let aes_key = crypto::generate_aes_key();
        let ciphertext = crypto::encrypt_aes_ecb(data, &aes_key);

        let mut filekey_buf = [0u8; 16];
        rand::rng().fill_bytes(&mut filekey_buf);
        let filekey = hex::encode(filekey_buf);

        let raw_md5 = hex::encode(Md5::digest(data));

        let params = protocol::GetUploadUrlParams {
            filekey: filekey.clone(),
            media_type,
            to_user_id: user_id.to_string(),
            rawsize: data.len(),
            rawfilemd5: raw_md5,
            filesize: ciphertext.len(),
            no_need_thumb: true,
            aeskey: crypto::encode_aes_key_hex(&aes_key),
        };

        // ---- 日志：getuploadurl 请求参数 ----
        let log_path = get_desktop_log_path("rust.log");
        let _ = append_log(&log_path, &format!(
            "\n[CDN_UPLOAD] media_type={}\n  getuploadurl params: {:?}",
            media_type,
            serde_json::to_string(&params).unwrap_or_default()
        ));

        let upload_resp = self.client.get_upload_url(base_url, token, &params).await?;

        // ---- 日志：getuploadurl 响应 ----
        let _ = append_log(&log_path, &format!(
            "  getuploadurl response: upload_param={:?}, upload_full_url={:?}",
            upload_resp.upload_param,
            upload_resp.upload_full_url
        ));

        let upload_url = if let Some(ref full_url) = upload_resp.upload_full_url {
            if !full_url.trim().is_empty() {
                let _ = append_log(&log_path, &format!("  using upload_full_url: {}", full_url));
                full_url.clone()
            } else {
                let upload_param = upload_resp.upload_param.ok_or_else(|| {
                    WeChatBotError::Media("getuploadurl returned no upload_param".into())
                })?;
                let url = protocol::build_cdn_upload_url(
                    protocol::CDN_BASE_URL,
                    &upload_param,
                    &filekey,
                );
                let _ = append_log(&log_path, &format!("  fallback to built url: {}", url));
                url
            }
        } else {
            let upload_param = upload_resp.upload_param.ok_or_else(|| {
                WeChatBotError::Media("getuploadurl returned no upload_param".into())
            })?;
            let url = protocol::build_cdn_upload_url(
                protocol::CDN_BASE_URL,
                &upload_param,
                &filekey,
            );
            let _ = append_log(&log_path, &format!("  no full_url, built url: {}", url));
            url
        };

        let encrypted_file_size = ciphertext.len();

        let encrypt_query_param = self.client.upload_to_cdn(&upload_url, &ciphertext).await?;

        // ---- 日志：上传结果 ----
        let _ = append_log(&log_path, &format!(
            "  CDN upload result: encrypt_query_param={}, encrypted_file_size={}",
            encrypt_query_param,
            encrypted_file_size
        ));

        let aes_key_b64 = crypto::encode_aes_key_base64(&aes_key);

        Ok(UploadResult {
            media: CDNMedia {
                encrypt_query_param,
                aes_key: aes_key_b64,
                encrypt_type: Some(1),
                full_url: None,
            },
            aes_key,
            encrypted_file_size,
        })
    }

    // --- internal text ---

    async fn send_text(&self, user_id: &str, text: &str, context_token: &str) -> Result<()> {
        let (base_url, token) = self.get_auth().await?;
        for chunk in chunk_text(text, 4000) {
            let msg = protocol::build_text_message(user_id, context_token, &chunk);
            self.client.send_message(&base_url, &token, &msg).await?;
        }
        Ok(())
    }

    async fn remember_context(&self, wire: &WireMessage) {
        let user_id = if wire.message_type == MessageType::User {
            &wire.from_user_id
        } else {
            &wire.to_user_id
        };
        if !user_id.is_empty() && !wire.context_token.is_empty() {
            self.context_tokens
                .write()
                .await
                .insert(user_id.clone(), wire.context_token.clone());
        }
    }

    async fn get_auth(&self) -> Result<(String, String)> {
        let creds = self.credentials.read().await;
        let creds = creds
            .as_ref()
            .ok_or_else(|| WeChatBotError::Auth("not logged in".into()))?;
        Ok((creds.base_url.clone(), creds.token.clone()))
    }

    async fn load_credentials(&self) -> Result<Option<Credentials>> {
        let path = self.cred_path.clone().unwrap_or_else(default_cred_path);
        match tokio::fs::read_to_string(&path).await {
            Ok(data) => Ok(Some(serde_json::from_str(&data)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn save_credentials(&self, creds: &Credentials) -> Result<()> {
        let path = self.cred_path.clone().unwrap_or_else(default_cred_path);
        let dir = std::path::Path::new(&path).parent().unwrap();
        tokio::fs::create_dir_all(dir).await?;
        let data = serde_json::to_string_pretty(creds)?;
        tokio::fs::write(&path, format!("{}\n", data)).await?;
        Ok(())
    }

    fn report_error(&self, err: &WeChatBotError) {
        error!("{}", err);
        if let Some(ref cb) = self.on_error {
            cb(err);
        }
    }
}

/// Content to send via reply_media / send_media.
pub enum SendContent {
    Text(String),
    Image {
        data: Vec<u8>,
        caption: Option<String>,
    },
    Video {
        data: Vec<u8>,
        caption: Option<String>,
    },
    File {
        data: Vec<u8>,
        file_name: String,
        caption: Option<String>,
    },
}

fn cdn_media_json(media: &CDNMedia) -> serde_json::Value {
    let mut v = json!({
        "encrypt_query_param": media.encrypt_query_param,
        "aes_key": media.aes_key,
    });
    if let Some(et) = media.encrypt_type {
        v["encrypt_type"] = json!(et);
    }
    v
}

fn categorize_by_extension(filename: &str) -> &'static str {
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => "image",
        "mp4" | "mov" | "webm" | "mkv" | "avi" => "video",
        _ => "file",
    }
}

fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= limit {
            chunks.push(remaining.to_string());
            break;
        }
        let window = &remaining[..limit];
        let cut = window
            .rfind("\n\n")
            .filter(|&i| i > limit * 3 / 10)
            .map(|i| i + 2)
            .or_else(|| {
                window
                    .rfind('\n')
                    .filter(|&i| i > limit * 3 / 10)
                    .map(|i| i + 1)
            })
            .or_else(|| {
                window
                    .rfind(' ')
                    .filter(|&i| i > limit * 3 / 10)
                    .map(|i| i + 1)
            })
            .unwrap_or(limit);
        chunks.push(remaining[..cut].to_string());
        remaining = &remaining[cut..];
    }
    if chunks.is_empty() {
        vec![String::new()]
    } else {
        chunks
    }
}

fn default_cred_path() -> String {
    let home = dirs_next::home_dir().unwrap_or_else(|| ".".into());
    home.join(".wechatbot")
        .join("credentials.json")
        .to_string_lossy()
        .to_string()
}

fn chrono_now() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    format!("{}Z", dur.as_secs())
}

// ── 日志辅助函数 ──

fn get_desktop_log_path(filename: &str) -> PathBuf {
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string())
    } else {
        std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
    };
    PathBuf::from(home).join("Desktop").join(filename)
}

fn append_log(path: &PathBuf, msg: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", msg)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_text_short() {
        let chunks = chunk_text("hello", 100);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn chunk_text_empty() {
        let chunks = chunk_text("", 100);
        assert_eq!(chunks, vec![""]);
    }

    #[test]
    fn chunk_text_splits_on_paragraph() {
        let text = "aaaa\n\nbbbb";
        let chunks = chunk_text(text, 7);
        assert_eq!(chunks, vec!["aaaa\n\n", "bbbb"]);
    }

    #[test]
    fn chunk_text_splits_on_newline() {
        let text = "aaaa\nbbbb";
        let chunks = chunk_text(text, 7);
        assert_eq!(chunks, vec!["aaaa\n", "bbbb"]);
    }

    #[test]
    fn chunk_text_exact_limit() {
        let text = "abcdef";
        let chunks = chunk_text(text, 6);
        assert_eq!(chunks, vec!["abcdef"]);
    }

    #[test]
    fn default_cred_path_not_empty() {
        let path = default_cred_path();
        assert!(!path.is_empty());
        assert!(path.contains(".wechatbot"));
        assert!(path.contains("credentials.json"));
    }

    #[test]
    fn categorize_image_extensions() {
        assert_eq!(categorize_by_extension("photo.png"), "image");
        assert_eq!(categorize_by_extension("photo.JPG"), "image");
        assert_eq!(categorize_by_extension("anim.gif"), "image");
        assert_eq!(categorize_by_extension("pic.webp"), "image");
    }

    #[test]
    fn categorize_video_extensions() {
        assert_eq!(categorize_by_extension("clip.mp4"), "video");
        assert_eq!(categorize_by_extension("clip.MOV"), "video");
        assert_eq!(categorize_by_extension("movie.webm"), "video");
    }

    #[test]
    fn categorize_file_extensions() {
        assert_eq!(categorize_by_extension("report.pdf"), "file");
        assert_eq!(categorize_by_extension("data.csv"), "file");
        assert_eq!(categorize_by_extension("noext"), "file");
    }

    #[test]
    fn cdn_media_json_with_encrypt_type() {
        let media = CDNMedia {
            encrypt_query_param: "param=1".to_string(),
            aes_key: "key123".to_string(),
            encrypt_type: Some(1),
            full_url: None,
        };
        let j = cdn_media_json(&media);
        assert_eq!(j["encrypt_query_param"], "param=1");
        assert_eq!(j["aes_key"], "key123");
        assert_eq!(j["encrypt_type"], 1);
    }

    #[test]
    fn cdn_media_json_without_encrypt_type() {
        let media = CDNMedia {
            encrypt_query_param: "p".to_string(),
            aes_key: "k".to_string(),
            encrypt_type: None,
            full_url: None,
        };
        let j = cdn_media_json(&media);
        assert!(j.get("encrypt_type").is_none());
    }
}
