// main.rs
// n8n Bot — Rust 版本，连接 wechatbot 与 n8n webhook
//
// 依赖（Cargo.toml 中需添加）：
//   wechatbot = "0.1"         （你的库路径）
//   tokio = { version = "1", features = ["full"] }
//   reqwest = { version = "0.12", features = ["json"] }
//   serde_json = "1"
//   tempfile = "3"
//   tracing-subscriber = "0.3" （可选）
//   tracing = "0.1"            （可选）

use std::sync::Arc;
use std::time::Duration;
use tempfile::Builder;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use wechatbot::{
    BotOptions, ContentType, DownloadedMedia, IncomingMessage, SendContent, WeChatBot,
};

const N8N_WEBHOOK_URL: &str = "http://localhost:5678/webhook/wechat-bot";
const N8N_TIMEOUT_MS: u64 = 120_000; // 2 分钟

#[tokio::main]
async fn main() {
    // 初始化日志（可选）
    tracing_subscriber::fmt::init();

    // 创建并登录 bot
    let bot = Arc::new(WeChatBot::new(BotOptions {
        on_qr_url: Some(Box::new(|url| {
            println!("\nScan this URL in WeChat:\n{}\n", url);
        })),
        on_error: Some(Box::new(|err| {
            eprintln!("Bot error: {}", err);
        })),
        ..Default::default()
    }));

    let creds = bot
        .login(false)
        .await
        .expect("登录失败");
    println!("Logged in: {} ({})", creds.account_id, creds.user_id);

    // 注册消息处理器（需要将 Arc<WeChatBot> 传入异步任务）
    let bot_for_handler = Arc::clone(&bot);
    bot.on_message(Box::new(move |msg| {
        let bot = Arc::clone(&bot_for_handler);
        let msg = msg.clone(); // 获取所有权，以便传入 spawn

        tokio::spawn(async move {
            if let Err(e) = handle_message(bot, msg).await {
                eprintln!("处理消息出错: {}", e);
            }
        });
    }))
    .await;

    println!("Listening for messages (Ctrl+C to stop)");
    bot.run().await.expect("运行失败");
}

/// 处理单条消息：下载媒体 → 转发到 n8n → 按回复发送内容
async fn handle_message(bot: Arc<WeChatBot>, msg: IncomingMessage) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 发送 "正在输入" 状态
    let _ = bot.send_typing(&msg.user_id).await;

    // 2. 构建要发给 n8n 的基础 JSON
    let mut payload = serde_json::json!({
        "message": {
            "userId": msg.user_id,
            "text": msg.text,
            "type": content_type_str(&msg.content_type),
            "timestamp": msg.timestamp
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as u64,
        }
    });

    // 3. 根据消息类型处理媒体（下载到临时文件）
    //    临时文件的路径需要保持有效，直到 n8n 处理完，所以调用 .keep() 持久化
    let mut _tempfile: Option<tempfile::NamedTempFile> = None;

    match msg.content_type {
        ContentType::Image | ContentType::Video | ContentType::File => {
            if let Ok(Some(media)) = bot.download(&msg).await {
                let (ext, media_type) = match msg.content_type {
                    ContentType::Image => (".jpg", "image"),
                    ContentType::Video => (".mp4", "video"),
                    _ => ("", "file"), // File 保留原文件名
                };

                let file_name = media
                    .file_name
                    .as_deref()
                    .unwrap_or(ext)
                    .to_string();

                // 创建临时文件
                let tmp = Builder::new()
                    .prefix("n8n-")
                    .suffix(&format!("-{}", file_name))
                    .tempfile()?;
                let path = tmp.path().to_path_buf();

                // 写入媒体数据
                let mut file = fs::File::create(&path).await?;
                file.write_all(&media.data).await?;
                file.sync_all().await?;

                // 保留文件，防止自动删除
                let kept = tmp.keep()?;
                let path_str = kept.to_string_lossy().to_string();
                _tempfile = Some(kept); // 将文件所有权保留到函数结束（避免过早删除）

                // 补充 payload 信息
                payload["message"]["mediaPath"] = serde_json::json!(path_str);
                payload["message"]["mediaType"] = serde_json::json!(media_type);
                payload["message"]["mediaSize"] = serde_json::json!(media.data.len());
            }
        }
        ContentType::Voice => {
            // 语音：提取第一条语音的文本与时长
            if let Some(voice) = msg.voices.first() {
                payload["message"]["voiceText"] = serde_json::json!(voice.text.clone().unwrap_or_default());
                payload["message"]["voiceDuration"] =
                    serde_json::json!(voice.duration_ms.unwrap_or(0));
            }
        }
        _ => { /* 纯文本无需额外处理 */ }
    }

    // 4. 调用 n8n webhook
    println!(
        "→ Forwarding to n8n: {} {}",
        payload["message"]["type"].as_str().unwrap_or("?"),
        payload["message"]["text"].as_str().unwrap_or("(media)"),
    );

    let client = reqwest::Client::new();
    let response = client
        .post(N8N_WEBHOOK_URL)
        .json(&payload)
        .timeout(Duration::from_millis(N8N_TIMEOUT_MS))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        return Err(format!("n8n 返回 HTTP {status}").into());
    }

    let result: serde_json::Value = response.json().await?;

    // 5. 根据 n8n 回复发送消息
    if let Some(file_path) = result["filePath"].as_str() {
        // n8n 要求发送文件
        let file_path = file_path.to_string();
        let file_name = result["fileName"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                std::path::Path::new(&file_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string()
            });

        match fs::read(&file_path).await {
            Ok(data) => {
                bot.reply_media(
                    &msg,
                    SendContent::File {
                        data,
                        file_name,
                        caption: None,
                    },
                )
                .await?;
                println!("← Sent file: {}", file_path);
            }
            Err(e) => {
                let error_text = format!("发送文件失败：{}", e);
                bot.reply(&msg, &error_text).await?;
                println!("← Error: {}", error_text);
            }
        }
    } else {
        // 发送文本回复
        let reply_text = result["reply"]
            .as_str()
            .unwrap_or("n8n 未返回有效回复")
            .to_string();
        bot.reply(&msg, &reply_text).await?;
        println!("← Replied: {}", reply_text);
    }

    Ok(())
}

/// 将 ContentType 转为字符串，方便 JSON 序列化
fn content_type_str(ct: &ContentType) -> &'static str {
    match ct {
        ContentType::Text => "text",
        ContentType::Image => "image",
        ContentType::Voice => "voice",
        ContentType::File => "file",
        ContentType::Video => "video",
    }
}
