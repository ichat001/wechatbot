// examples/n8n_bot.rs
// n8n Bot — Rust 版本6，连接 wechatbot 与 n8n webhook
// https://chat.deepseek.com/a/chat/s/3f0eb79e-7029-4877-9c95-90af65240239

use std::env;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use wechatbot::{
    BotOptions, ContentType, IncomingMessage, SendContent, WeChatBot,
};
use chrono::DateTime; // 新增：用于时间格式化

const N8N_WEBHOOK_URL: &str = "http://localhost:5678/webhook/wechat-bot";
const N8N_TIMEOUT_MS: u64 = 120_000; // 2 分钟

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

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

    let bot_for_handler = Arc::clone(&bot);
    bot.on_message(Box::new(move |msg| {
        let bot = Arc::clone(&bot_for_handler);
        let msg = msg.clone();
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

async fn handle_message(
    bot: Arc<WeChatBot>,
    msg: IncomingMessage,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. 发送 "正在输入" 状态
    let _ = bot.send_typing(&msg.user_id).await;

    // --- 修改: 提前计算时间戳（毫秒）供文件名和 payload 共用 ---
    let timestamp_millis = msg
        .timestamp
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;

    // 2. 构建 payload
    let mut payload = serde_json::json!({
        "message": {
            "userId": msg.user_id,
            "text": msg.text,
            "type": content_type_str(&msg.content_type),
            "timestamp": timestamp_millis,
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

                // 修改: 将时间戳格式化为 年月日时分秒 用作文件名
                let time_str = {
                    let secs = msg
                        .timestamp
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let datetime = DateTime::from_timestamp(secs as i64, 0)
                        .unwrap_or_default();
                    datetime.format("%Y%m%d%H%M%S").to_string()
                };

                let file_name = if let Some(original) = &media.file_name {
                    original.clone()
                } else {
                    format!("{}_{}{}", media_type, time_str, ext)
                };

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
        let count = files_to_send.len();

        for (i, file_path) in files_to_send.iter().enumerate() {
            match fs::read(&file_path).await {
                Ok(data) => {
                    let file_name = Path::new(file_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file");
                    let ext = Path::new(file_path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();

                    match ext.as_str() {
                        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" => {
                            // 图片消息（命名字段结构体）
                            bot.reply_media(&msg, SendContent::Image {
                                data,
                                caption: None,   // 图片不支持直接附带文本，稍后单独发送
                            }).await?;
                            println!("← Sent image: {}", file_path);
                        }
                        "mp4" | "mov" | "avi" => {
                            // 视频消息
                            bot.reply_media(&msg, SendContent::Video {
                                data,
                                caption: None,
                            }).await?;
                            println!("← Sent video: {}", file_path);
                        }
                        _ => {
                            // 普通文件
                            let caption = if i == count - 1 && !reply_text.is_empty() {
                                Some(reply_text.clone())
                            } else {
                                None
                            };
                            bot.reply_media(&msg, SendContent::File {
                                data,
                                file_name: file_name.to_string(),
                                caption,
                            }).await?;
                            println!("← Sent file: {}", file_path);
                        }
                    }
                }
                Err(e) => {
                    let error_text = format!("发送文件 {} 失败：{}", file_path, e);
                    bot.reply(&msg, &error_text).await?;
                    println!("← Error: {}", error_text);
                }
            }
        }

        // 如果最后一个是图片/视频且需要附带文本，单独发送一条文本消息
        if !reply_text.is_empty() {
            let last_ext = files_to_send
                .last()
                .and_then(|p| Path::new(p).extension())
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            let media_exts = ["jpg", "jpeg", "png", "gif", "bmp", "webp", "mp4", "mov", "avi"];
            if media_exts.contains(&last_ext.as_str()) {
                bot.reply(&msg, &reply_text).await?;
                println!("← Text after media: {}", reply_text);
            }
        }
    } else {
        bot.reply(&msg, &reply_text).await?;
        println!("← Replied: {}", reply_text);
    }

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
