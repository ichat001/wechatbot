// examples/n8n_bot.rs
// n8n Bot — Rust 版本，连接 wechatbot 与 n8n webhook

use std::env;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use wechatbot::{
    crypto::encode_aes_key_hex,
    protocol::{build_media_message, ILinkClient},
    BotOptions, ContentType, IncomingMessage, SendContent, WeChatBot,
};

const N8N_WEBHOOK_URL: &str = "http://localhost:5678/webhook/wechat-bot";
const N8N_TIMEOUT_MS: u64 = 120_000; // 2 分钟

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let bot = Arc::new(WeChatBot::new(BotOptions {
        on_qr_url: Some(Box::new(|url| {
            println!("\nScan this URL in WeChat:\n{}\n", url);
        })),
        on_error: Some(Box::new(|err| {
            eprintln!("Bot error: {}", err);
        })),
        ..Default::default()
    }));

    let creds = bot.login(false).await.expect("登录失败");
    println!("Logged in: {} ({})", creds.account_id, creds.user_id);

    // 保存 base_url 和 token 供自定义媒体发送使用
    let auth = Arc::new((creds.base_url.clone(), creds.token.clone()));

    let bot_for_handler = Arc::clone(&bot);
    bot.on_message(Box::new(move |msg| {
        let bot = Arc::clone(&bot_for_handler);
        let auth = Arc::clone(&auth);
        let msg = msg.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_message(bot, auth, msg).await {
                eprintln!("处理消息出错: {}", e);
            }
        });
    }))
    .await;

    println!("Listening for messages (Ctrl+C to stop)");
    bot.run().await.expect("运行失败");
}

/// 根据文件扩展名返回媒体类型，与库内 `categorize_by_extension` 一致。
fn media_type_from_path(path: &Path) -> &'static str {
    let ext = path
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

async fn handle_message(
    bot: Arc<WeChatBot>,
    auth: Arc<(String, String)>,
    msg: IncomingMessage,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 发送 "正在输入" 状态
    let _ = bot.send_typing(&msg.user_id).await;

    // 2. 构建 payload
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

    // 3. 处理媒体文件
    match msg.content_type {
        ContentType::Image | ContentType::Video | ContentType::File => {
            if let Ok(Some(media)) = bot.download(&msg).await {
                let (ext, media_type) = match msg.content_type {
                    ContentType::Image => (".jpg", "image"),
                    ContentType::Video => (".mp4", "video"),
                    _ => ("", "file"),
                };
                let file_name = media.file_name.as_deref().unwrap_or(ext).to_string();

                // 保存到用户目录下的 .n8n/www/<userId> 目录
                let home = if cfg!(windows) {
                    env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string())
                } else {
                    env::var("HOME").unwrap_or_else(|_| ".".to_string())
                };
                let base_dir = Path::new(&home).join(".n8n").join("www").join(&msg.user_id);
                fs::create_dir_all(&base_dir).await?;

                let file_path = base_dir.join(&file_name);

                let mut file = fs::File::create(&file_path).await?;
                file.write_all(&media.data).await?;
                file.sync_all().await?;

                // 转换为正斜杠路径
                let path_str = file_path.to_string_lossy().replace("\\", "/");
                payload["message"]["mediaPath"] = serde_json::json!(path_str);
                payload["message"]["mediaType"] = serde_json::json!(media_type);
                payload["message"]["mediaSize"] = serde_json::json!(media.data.len());
            }
        }
        ContentType::Voice => {
            if let Some(voice) = msg.voices.first() {
                payload["message"]["voiceText"] =
                    serde_json::json!(voice.text.clone().unwrap_or_default());
                payload["message"]["voiceDuration"] =
                    serde_json::json!(voice.duration_ms.unwrap_or(0));
            }
        }
        _ => {}
    }

    // 4. 转发到 n8n
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
        return Err(format!("n8n 返回 HTTP {}", response.status()).into());
    }

    let result: serde_json::Value = response.json().await?;

    // AI Agent 输出为数组，取第一个元素
    let output_obj = if let Some(arr) = result.as_array() {
        arr.first().cloned().unwrap_or_default()
    } else {
        result.clone()
    };
    let output_text = output_obj["output"].as_str().unwrap_or("").to_string();

    // 解析 FILE: 指令
    let mut files_to_send: Vec<String> = Vec::new();
    let mut reply_text = String::new();

    if let Some(first_line) = output_text.lines().next() {
        if let Some(paths_str) = first_line.strip_prefix("FILE:") {
            files_to_send = paths_str
                .split(';')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let remaining: Vec<&str> = output_text.lines().skip(1).collect();
            reply_text = remaining.join("\n");
        } else {
            reply_text = output_text;
        }
    } else {
        reply_text = output_text;
    }

    // 5. 根据解析结果发送消息
    if !files_to_send.is_empty() {
        for (i, file_path) in files_to_send.iter().enumerate() {
            let path = Path::new(file_path);
            // 检查文件是否存在
            if !path.exists() {
                let err_text = format!("文件不存在: {}", file_path);
                eprintln!("← Error: {}", err_text);
                bot.reply(&msg, &err_text).await?;
                continue;
            }
            // 检查是否为文件
            if !path.is_file() {
                let err_text = format!("路径不是文件: {}", file_path);
                eprintln!("← Error: {}", err_text);
                bot.reply(&msg, &err_text).await?;
                continue;
            }

            match fs::read(&file_path).await {
                Ok(data) => {
                    let file_size = data.len();
                    if file_size == 0 {
                        let err_text = format!("文件为空: {}", file_path);
                        eprintln!("← Error: {}", err_text);
                        bot.reply(&msg, &err_text).await?;
                        continue;
                    }

                    let file_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_string();

                    // caption 只在最后一个文件上附加
                    let caption = if i == files_to_send.len() - 1 && !reply_text.is_empty() {
                        Some(reply_text.clone())
                    } else {
                        None
                    };

                    // === 根据扩展名采用不同的发送策略 ===
                    let media_type = media_type_from_path(path);

                    match media_type {
                        "image" => {
                            if let Err(e) = send_image_or_video(
                                &bot,
                                &auth,
                                &msg,
                                data,
                                1, // 1 = image
                                caption,
                                file_path,
                                file_size,
                            )
                            .await
                            {
                                let err_text = format!("发送图片 {} 失败：{}", file_path, e);
                                eprintln!("← Error: {}", err_text);
                                bot.reply(&msg, &err_text).await?;
                            }
                        }
                        "video" => {
                            if let Err(e) = send_image_or_video(
                                &bot,
                                &auth,
                                &msg,
                                data,
                                2, // 2 = video
                                caption,
                                file_path,
                                file_size,
                            )
                            .await
                            {
                                let err_text = format!("发送视频 {} 失败：{}", file_path, e);
                                eprintln!("← Error: {}", err_text);
                                bot.reply(&msg, &err_text).await?;
                            }
                        }
                        _ => {
                            // 普通文件：继续使用库自带的 reply_media
                            let content = SendContent::File {
                                data,
                                file_name: file_name.clone(),
                                caption,
                            };
                            match bot.reply_media(&msg, content).await {
                                Ok(_) => {
                                    println!("← Sent file: {} ({} bytes)", file_path, file_size);
                                }
                                Err(e) => {
                                    let err_text = format!("发送文件 {} 失败：{}", file_path, e);
                                    eprintln!("← Error: {}", err_text);
                                    bot.reply(&msg, &err_text).await?;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let error_text = format!("读取文件 {} 失败：{}", file_path, e);
                    eprintln!("← Error: {}", error_text);
                    bot.reply(&msg, &error_text).await?;
                }
            }
        }
    } else {
        bot.reply(&msg, &reply_text).await?;
        println!("← Replied: {}", reply_text);
    }

    Ok(())
}

/// 自定义发送图片/视频，绕过库中缺失 `aeskey` 的问题。
async fn send_image_or_video(
    bot: &WeChatBot,
    auth: &(String, String),
    msg: &IncomingMessage,
    data: Vec<u8>,
    media_type_i32: i32, // 1: image, 2: video
    caption: Option<String>,
    file_path: &str,
    file_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 上传媒体到 CDN，获取加密信息和密钥
    let upload_result = bot.upload(&data, &msg.user_id, media_type_i32).await?;

    // 2. 构建 item_list（可选的 caption 文本 + 媒体 item）
    let mut items: Vec<serde_json::Value> = Vec::new();
    if let Some(cap) = caption {
        items.push(serde_json::json!({"type": 1, "text_item": {"text": cap}}));
    }

    let media_json = serde_json::json!({
        "encrypt_query_param": upload_result.media.encrypt_query_param,
        "aes_key": upload_result.media.aes_key,
        "encrypt_type": 1,
    });

    if media_type_i32 == 1 {
        // 图片必须携带 aeskey（AES 密钥的 hex 字符串）
        let aeskey_hex = encode_aes_key_hex(&upload_result.aes_key);
        items.push(serde_json::json!({
            "type": 2,
            "image_item": {
                "media": media_json,
                "mid_size": upload_result.encrypted_file_size,
                "aeskey": aeskey_hex
            }
        }));
    } else {
        // 视频：添加 play_length 占位，避免消息被拒绝
        items.push(serde_json::json!({
            "type": 5,
            "video_item": {
                "media": media_json,
                "video_size": upload_result.encrypted_file_size,
                "play_length": 0
            }
        }));
    }

    // 3. 构建并发送消息（context_token 是方法，需加括号调用）
    let payload = build_media_message(&msg.user_id, &msg.context_token(), items);
    let ilink_client = ILinkClient::new();
    ilink_client.send_message(&auth.0, &auth.1, &payload).await?;

    let type_label = if media_type_i32 == 1 { "image" } else { "video" };
    println!("← Sent {}: {} ({} bytes)", type_label, file_path, file_size);
    Ok(())
}

fn content_type_str(ct: &ContentType) -> &'static str {
    match ct {
        ContentType::Text => "text",
        ContentType::Image => "image",
        ContentType::Voice => "voice",
        ContentType::File => "file",
        ContentType::Video => "video",
    }
}
