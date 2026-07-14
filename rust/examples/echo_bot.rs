use std::sync::Arc;

use wechatbot::{BotOptions, WeChatBot};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let bot = Arc::new(WeChatBot::new(BotOptions {
        on_qr_url: Some(Box::new(|url| {
            println!("\nScan this URL in WeChat:\n{}\n", url);
        })),
        on_error: Some(Box::new(|err| {
            eprintln!("Error: {}", err);
        })),
        ..Default::default()
    }));

    let creds = bot.login(false).await.expect("login failed");

    println!(
        "Logged in: {} ({})",
        creds.account_id,
        creds.user_id
    );

    let bot2 = bot.clone();

    bot.on_message(Box::new(move |msg| {
        println!(
            "[{}] {}: {}",
            msg.content_type_str(),
            msg.user_id,
            msg.text
        );

        let bot = bot2.clone();
        let msg = msg.clone();

        tokio::spawn(async move {
            if let Err(e) = bot.reply(&msg, &msg.text).await {
                eprintln!("Reply failed: {}", e);
            }
        });
    }))
    .await;

    println!("Listening for messages (Ctrl+C to stop)");

    bot.run().await.expect("run failed");
}

trait ContentTypeStr {
    fn content_type_str(&self) -> &str;
}

impl ContentTypeStr for wechatbot::IncomingMessage {
    fn content_type_str(&self) -> &str {
        match self.content_type {
            wechatbot::ContentType::Text => "text",
            wechatbot::ContentType::Image => "image",
            wechatbot::ContentType::Voice => "voice",
            wechatbot::ContentType::File => "file",
            wechatbot::ContentType::Video => "video",
        }
    }
}
