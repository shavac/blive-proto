# blive-proto

Bilibili live room danmaku WebSocket, Protobuf, command, and authentication protocol library in Rust.

## Features

- **Async Bilibili REST Client** (`api::BiliApiClient`): Encapsulates QR login generate/poll, session validation (`nav` + live user-info fallback), danmaku sending (`/msg/send`), user/following live room queries, room metadata resolution (`get_info` / `getInfoByRoom` / `room_init`), `buvid3` resolution, and active SuperChat seeding.
- **WebSocket Framing, Handshake & Managed Session** (`websocket`): Panic-free binary frame decoder (`decode_bili_frames`: plain JSON, Zlib `ver=2`, Brotli `ver=3`), managed blocking `LiveWsSession` (buvid3 fallback, WBI `getDanmuInfo`, `protover=3` AUTH, 30s HEARTBEAT), plus standalone `BiliLiveClient` helpers.
- **Schema-Driven Protobuf Decoding** (`pb`): Compile-time code generation from [`proto/event.proto`](proto/event.proto) via `prost-build` and `protoc-bin-vendored` (no system `protoc` installation required), supporting `INTERACT_WORD_V2`, `SEND_GIFT_V2`, `ONLINE_RANK_V2`/`V3`, and `DANMU_MSG` `dm_v2`, plus `enrich_raw_json_with_pb_decoded`.
- **Stateless Parsers & Stateful Normalizer** (`commands`): Room metadata/REST response parsers, `apply_room_meta_cmd`, `parse_bili_cmd_events` -> `Vec<RawBiliEvent>`, and `EventNormalizer` (monotonic `seq`, LRU avatar backfill, `GUARD_BUY` dedup, `payflow_id` dedup).
- **Auth & Danmaku Send Protocol Helpers** (`auth`): Session cookie validation/parsing (`SessionData`), WBI parameter signing, QR login polling response parsers (`QrPollStatus`), and danmaku send response handling (`SendDanmakuError`).

## Installation

Add `blive-proto` to your `Cargo.toml`:

```toml
[dependencies]
blive-proto = "0.2"
```

Because `blive-proto` bundles `protoc-bin-vendored`, building on Linux, macOS, and Windows works out of the box without installing `protoc` separately (if `PROTOC` is set in the environment, it is respected).

## Quick Example

```rust
use blive_proto::commands::{parse_bili_cmd_events, parse_room_input};
use blive_proto::websocket::decode_bili_frames;
use serde_json::Value;

let room_id = parse_room_input("https://live.bilibili.com/21465419").unwrap();

// Decode incoming WebSocket binary bytes into JSON command packets:
let mut packets: Vec<Value> = Vec::new();
# let ws_bytes: &[u8] = &[];
let frame_res = decode_bili_frames(ws_bytes, &mut packets);
let _auth_ok = frame_res.auth_ack;

for pkt in &packets {
    let cmd = pkt["cmd"].as_str().unwrap_or("");
    let events = parse_bili_cmd_events(cmd, pkt, room_id, "Streamer");
    for ev in events {
        println!("[{}] {}: {}", ev.kind, ev.uname, ev.content);
    }
}
```

## Supported Protocol Commands

| Command | Payload Type | Normalized `kind` / Handling |
| :--- | :--- | :--- |
| `DANMU_MSG` | JSON (+ optional `dm_v2` PB) | `chat` (text, stickers, inline emots) |
| `SEND_GIFT` | JSON | `gift` (gold/silver coin, combo metadata) |
| `SEND_GIFT_V2` | Protobuf (`EventSendGift`) | `gift` (multi-item, blind gift, medal, guard) |
| `COMBO_SEND` / `GIFT_COMBO` | JSON | `combo_end` |
| `INTERACT_WORD` | JSON | `enter`, `follow`, `share`, `special_follow`, `mutual_follow` |
| `INTERACT_WORD_V2` | Protobuf (`EventInteractWord`) | `enter`, `follow`, `share`, `special_follow`, `mutual_follow` |
| `SUPER_CHAT_MESSAGE` | JSON | `superchat` |
| `SUPER_CHAT_MESSAGE_DELETE` | JSON | `superchat_delete` |
| `USER_TOAST_MSG` / `USER_TOAST_MSG_V2` | JSON / nested JSON | `guard` |
| `ENTRY_EFFECT` | JSON | `guard_enter` |
| `LIKE_INFO_V3_CLICK` | JSON | `like` |
| `ONLINE_RANK_V2` / `ONLINE_RANK_V3` | Protobuf (`EventOnlineRank`) | Decodable via `pb::decode_online_rank` & `enrich_raw_json_with_pb_decoded` |
| `ONLINE_RANK_COUNT` | JSON | `BiliMessage::OnlineRankCount` in `websocket::handle` |
| `LIVE` / `PREPARING` / `ROOM_CHANGE` / `ROOM_SILENT_*` / `ROOM_BLOCK_MSG` / `WARNING` / `CUT_OFF` | JSON | Room lifecycle & moderation events |

## License

Licensed under the [MIT License](LICENSE).
