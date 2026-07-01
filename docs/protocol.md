# WeChat iLink Bot API Protocol Reference

Base URL: `https://ilinkai.weixin.qq.com`
CDN URL: `https://novac2c.cdn.weixin.qq.com/c2c`

## Authentication (QR Login)

### Step 1: Get QR Code
```
GET /ilink/bot/get_bot_qrcode?bot_type=3
→ { qrcode: "<token>", qrcode_img_content: "<url>" }
```

### Step 2: Poll Status
```
GET /ilink/bot/get_qrcode_status?qrcode=<token>
Headers: { "iLink-App-ClientVersion": "1" }
→ { status: "wait" | "scaned" | "confirmed" | "expired", bot_token?, ilink_bot_id?, ilink_user_id?, baseurl? }
```

## Common Headers (all POST requests)
```
Content-Type: application/json
AuthorizationType: ilink_bot_token
Authorization: Bearer <bot_token>
X-WECHAT-UIN: <base64(String(randomUint32))>
```
All POST bodies include: `base_info: { channel_version: "<version>" }`

## Get Updates (Long Poll)
```
POST /ilink/bot/getupdates
Body: { get_updates_buf: "<cursor>", base_info: {...} }
Timeout: 35s (server holds connection)
→ { ret: 0, msgs: [], get_updates_buf: "<new_cursor>" }
```
Error: `errcode: -14` = session expired (re-login needed)

## Send Message
```
POST /ilink/bot/sendmessage
Body: {
  msg: {
    from_user_id: "",
    to_user_id: "<user_id>",
    client_id: "<uuid>",
    message_type: 2,        // BOT
    message_state: 2,       // FINISH
    context_token: "<from_inbound_msg>",
    item_list: [{ type: 1, text_item: { text: "..." } }]
  },
  base_info: {...}
}
```

## Send Typing
```
POST /ilink/bot/getconfig
→ { typing_ticket: "<ticket>" }

POST /ilink/bot/sendtyping
Body: { ilink_user_id: "<id>", typing_ticket: "<ticket>", status: 1|2, base_info: {...} }
```

## Connection Status Notify
```
POST /ilink/bot/msg/notifystart
Body: { base_info: {...} }
→ { ret: 0 }

POST /ilink/bot/msg/notifystop
Body: { base_info: {...} }
→ { ret: 0 }
```
Sent when the bot client starts / stops polling so the server can track per-account
online state. Failures are non-fatal — log and continue.

## Media Upload
```
POST /ilink/bot/getuploadurl
→ { upload_param: "<encrypted>" }

POST <cdn>/upload?encrypted_query_param=<param>&filekey=<key>
Content-Type: application/octet-stream
Body: AES-128-ECB encrypted bytes
Response Header: x-encrypted-param → download param
```

## Media Download
```
GET <cdn>/download?encrypted_query_param=<param>
→ AES-128-ECB encrypted bytes → decrypt with aes_key
```

## Message Item Types
| Type | Value | Description |
|------|-------|-------------|
| TEXT | 1 | Text content |
| IMAGE | 2 | Image with CDN media |
| VOICE | 3 | Voice with optional transcription |
| FILE | 4 | File attachment |
| VIDEO | 5 | Video with optional thumbnail |

## AES Key Formats
| Format | Example | Usage |
|--------|---------|-------|
| base64(raw 16 bytes) | `ABEiM0RVZneImaq7zN3u/w==` | CDNMedia.aes_key (format A) |
| base64(hex string) | `MDAxMTIyMzM0NDU1NjY3Nzg4OTlhYWJiY2NkZGVlZmY=` | CDNMedia.aes_key (format B) |
| direct hex (32 chars) | `00112233445566778899aabbccddeeff` | image_item.aeskey |

## Error Codes
| Code | Meaning | Action |
|------|---------|--------|
| `ret: 0` | Success | — |
| `errcode: -14` | Session expired | Re-login |
| `ret: -2` | Parameter error | Check request |

## context_token
- **Required** for every reply — routes messages to the correct conversation
- Cache per `(accountId, userId)` pair
- Persist across restarts
- Clear on session expiry / re-login
