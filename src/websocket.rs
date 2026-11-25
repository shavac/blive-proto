//! WebSocket client for Bilibili live danmaku messages (refactored from bili_live_dm)

use base64::{Engine as _, engine::general_purpose};
use flate2::read::ZlibDecoder;
use native_tls::TlsStream;
use serde_json::Value;
use std::io::{ErrorKind, Read as _};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket, client};

use url::Url;

use futures_channel::mpsc::Sender;
use http::Response;
use std::thread;

use crate::auth::*;
use crate::commands::{append_cookie_kv, extract_cookie_value, value_as_u64};
use crate::models::{AuthMessage, BiliMessage, DanmuServer, MsgHead};

pub struct BiliLiveClient {
    ws: WebSocket<TlsStream<TcpStream>>,
    cookies: String,
    room_id: String,
    auth_msg: String,
    ss: Sender<BiliMessage>,
}

impl BiliLiveClient {
    pub fn new(cookies: &str, room_id: &str, r: Sender<BiliMessage>) -> Self {
        let (ws, auth_msg) = Self::connect_with_auth(cookies, room_id)
            .unwrap_or_else(|e| panic!("Failed to create websocket client: {}", e));
        BiliLiveClient {
            ws,
            cookies: cookies.to_string(),
            room_id: room_id.to_string(),
            auth_msg,
            ss: r,
        }
    }

    /// Create a new client with explicit cookie validation.
    pub fn new_auto(
        cookies: Option<&str>,
        room_id: &str,
        r: Sender<BiliMessage>,
    ) -> Result<Self, String> {
        let resolved_cookies = resolve_cookies(cookies)?;
        let (ws, auth_msg) = Self::connect_with_auth(&resolved_cookies, room_id)?;
        Ok(BiliLiveClient {
            ws,
            cookies: resolved_cookies,
            room_id: room_id.to_string(),
            auth_msg,
            ss: r,
        })
    }

    /// Credentials resolved for this connection, also used for HTTP message sending.
    pub fn cookies(&self) -> &str {
        &self.cookies
    }

    pub fn send_auth(&mut self) {
        let payload = self.auth_msg.clone();
        if let Err(e) = self.send_packet_with_retry(&payload, Operation::AUTH, "auth") {
            log::error!("failed to send auth packet: {}", e);
        }
    }

    pub fn send_heart_beat(&mut self) {
        if let Err(e) = self.send_packet_with_retry("{}", Operation::HEARTBEAT, "heartbeat") {
            log::error!("failed to send heartbeat: {}", e);
        }
    }

    pub fn parse_ws_message(&mut self, resv: Vec<u8>) {
        let mut jsons = Vec::new();
        let res = decode_bili_frames(&resv, &mut jsons);
        if res.auth_ack {
            self.send_heart_beat();
        }
        if let Some(pop) = res.popularity {
            log::info!("popularity:{}", pop);
        }
        for val in jsons {
            if let Some(msg) = handle(val) {
                let _ = self.ss.try_send(msg);
            }
        }
    }

    pub fn parse_business_message(&mut self, h: MsgHead, b: &[u8]) {
        match h.operation {
            5 => match h.ver {
                3 => {
                    if let Ok(res) = decompress(b) {
                        self.parse_ws_message(res);
                    }
                }
                0 => {
                    if let Ok(res_json) = serde_json::from_slice::<Value>(b)
                        && let Some(msg) = handle(res_json)
                    {
                        let _ = self.ss.try_send(msg);
                    }
                }
                _ => log::error!("Unknown compression format"),
            },
            8 => self.send_heart_beat(),
            _ => log::error!("Unknown message format {}", h.operation),
        }
    }

    pub fn receive(&mut self) -> Result<(), String> {
        if !self.ws.can_read() {
            return Ok(());
        }
        match self.ws.read() {
            Ok(m) => {
                let res = m.into_data();
                if res.len() >= 16 {
                    self.parse_ws_message(res);
                }
                Ok(())
            }
            Err(e) => {
                let msg = format!("read msg error: {}", e);
                log::warn!("{}", msg);
                self.reconnect().map_err(|reconnect_err| {
                    format!("{}; reconnect failed: {}", msg, reconnect_err)
                })
            }
        }
    }

    fn connect_with_auth(
        cookies: &str,
        room_id: &str,
    ) -> Result<(WebSocket<TlsStream<TcpStream>>, String), String> {
        let (v, auth) = try_init_server(cookies, room_id)?;
        let (ws, _res) = connect_result(v["host_list"].clone())?;
        let auth_msg = serde_json::to_string(&auth)
            .map_err(|e| format!("serialize auth payload failed: {}", e))?;
        Ok((ws, auth_msg))
    }

    fn send_packet_with_retry(
        &mut self,
        body: &str,
        op: Operation,
        label: &str,
    ) -> Result<(), String> {
        let packet = make_packet(body, op);
        match self.ws.send(Message::Binary(packet.clone())) {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = format!("send {} error: {}", label, e);
                log::warn!("{}", msg);
                self.reconnect()?;
                self.ws
                    .send(Message::Binary(packet))
                    .map_err(|retry_err| format!("{}; resend {} failed: {}", msg, label, retry_err))
            }
        }
    }

    fn reconnect(&mut self) -> Result<(), String> {
        let backoff = [1_u64, 2, 5];
        let mut last_err = None;

        for (idx, delay_secs) in backoff.iter().enumerate() {
            if idx > 0 {
                thread::sleep(Duration::from_secs(*delay_secs));
            }

            match Self::connect_with_auth(&self.cookies, &self.room_id) {
                Ok((ws, auth_msg)) => {
                    self.ws = ws;
                    self.auth_msg = auth_msg;
                    let auth_resend = self.ws.send(Message::Binary(make_packet(
                        self.auth_msg.as_str(),
                        Operation::AUTH,
                    )));
                    let heartbeat_resend = self
                        .ws
                        .send(Message::Binary(make_packet("{}", Operation::HEARTBEAT)));

                    match (auth_resend, heartbeat_resend) {
                        (Ok(()), Ok(())) => {
                            log::info!(
                                "websocket reconnected on attempt {} for room {}",
                                idx + 1,
                                self.room_id
                            );
                            return Ok(());
                        }
                        (auth_result, heartbeat_result) => {
                            let reconnect_err = match (auth_result.err(), heartbeat_result.err()) {
                                (Some(a), Some(h)) => format!(
                                    "reconnected socket but auth resend failed: {}; heartbeat resend failed: {}",
                                    a, h
                                ),
                                (Some(a), None) => {
                                    format!("reconnected socket but auth resend failed: {}", a)
                                }
                                (None, Some(h)) => {
                                    format!("reconnected socket but heartbeat resend failed: {}", h)
                                }
                                (None, None) => unreachable!(),
                            };
                            log::warn!(
                                "websocket reconnect attempt {} did not fully recover for room {}: {}",
                                idx + 1,
                                self.room_id,
                                reconnect_err
                            );
                            last_err = Some(reconnect_err);
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "websocket reconnect attempt {} failed for room {}: {}",
                        idx + 1,
                        self.room_id,
                        e
                    );
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| "unknown reconnect failure".to_string()))
    }
}

pub fn gen_damu_list(list: &Value) -> Vec<DanmuServer> {
    let mut res: Vec<DanmuServer> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let host = s["host"].as_str().filter(|h| !h.trim().is_empty())?;
            Some(DanmuServer {
                host: host.to_string(),
                port: s["port"].as_u64().unwrap_or(2243) as i32,
                wss_port: s["wss_port"].as_u64().unwrap_or(443) as i32,
                ws_port: s["ws_port"].as_u64().unwrap_or(2244) as i32,
            })
        })
        .collect();
    if res.is_empty() {
        res.push(DanmuServer::default());
    }
    res
}

fn find_server(vd: Vec<DanmuServer>) -> (String, String, String) {
    let first = vd.into_iter().next().unwrap_or_default();
    let (host, wss_port) = (first.host, first.wss_port);
    (
        host.clone(),
        format!("{}:{}", host, wss_port),
        format!("wss://{}:{}/sub", host, wss_port),
    )
}

/// Panic-free version of [`init_server`] returning `Result<(Value, AuthMessage), String>`.
pub fn try_init_server(cookies: &str, room_id: &str) -> Result<(Value, AuthMessage), String> {
    let room_id_num = room_id
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("invalid room_id '{}': expected unsigned integer", room_id))?;

    let mut headers = reqwest::header::HeaderMap::new();
    if !cookies.trim().is_empty() {
        let cookie_val = reqwest::header::HeaderValue::from_str(cookies)
            .map_err(|e| format!("invalid Cookie header value: {}", e))?;
        headers.insert(reqwest::header::COOKIE, cookie_val);
    }
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static(crate::auth::USER_AGENT),
    );
    log::debug!("headers: {:?}", headers);

    let uid = if extract_cookie_value(cookies, "SESSDATA").is_some() {
        let (_, body1) = try_init_uid(headers.clone())?;
        let body1_v: Value = serde_json::from_str(&body1)
            .map_err(|e| format!("failed to parse nav UID response JSON: {}", e))?;

        if let Some(mid) = value_as_u64(&body1_v["data"]["mid"]) {
            log::info!("Successfully authenticated with uid: {}", mid);
            mid
        } else {
            log::warn!("Authentication failed - SESSDATA may be invalid or expired");
            log::debug!("Auth response: {}", body1);
            0
        }
    } else {
        0
    };

    let (_, body4) = try_init_host_server(headers, room_id_num)?;
    let body4_res: Value = serde_json::from_str(&body4)
        .map_err(|e| format!("failed to parse getDanmuInfo response JSON: {}", e))?;
    let code = body4_res["code"].as_i64().unwrap_or(-1);
    if code != 0 {
        return Err(format!(
            "getDanmuInfo API failed (code {}): {}",
            code,
            body4_res["message"].as_str().unwrap_or("unknown")
        ));
    }
    let server_info = &body4_res["data"];
    let token = server_info["token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "getDanmuInfo response missing data.token".to_string())?;

    let auth_msg = AuthMessage {
        uid,
        roomid: room_id_num,
        protover: 3,
        platform: "web".to_string(),
        type_: 2,
        key: token.to_string(),
    };
    Ok((server_info.clone(), auth_msg))
}

pub fn init_server(cookies: &str, room_id: &str) -> (Value, AuthMessage) {
    try_init_server(cookies, room_id).unwrap_or_else(|e| panic!("{}", e))
}

pub fn connect(v: Value) -> (WebSocket<TlsStream<TcpStream>>, Response<Option<Vec<u8>>>) {
    connect_result(v).expect("Can't connect")
}

pub type WsConnectStream = (WebSocket<TlsStream<TcpStream>>, Response<Option<Vec<u8>>>);

pub fn connect_result(v: Value) -> Result<WsConnectStream, String> {
    let danmu_server = gen_damu_list(&v);
    let (host, url, ws_url) = find_server(danmu_server);
    let connector =
        native_tls::TlsConnector::new().map_err(|e| format!("tls init failed: {}", e))?;
    let stream = TcpStream::connect(url.as_str())
        .map_err(|e| format!("tcp connect to {} failed: {}", url, e))?;
    let stream = connector
        .connect(host.as_str(), stream)
        .map_err(|e| format!("tls connect to {} failed: {}", host, e))?;
    let parsed_url =
        Url::parse(ws_url.as_str()).map_err(|e| format!("invalid websocket url: {}", e))?;
    client(parsed_url, stream).map_err(|e| format!("websocket handshake failed: {}", e))
}

pub enum Operation {
    AUTH,
    HEARTBEAT,
}

pub fn make_packet(body: &str, ops: Operation) -> Vec<u8> {
    let serialized = serde_json::from_str::<Value>(body)
        .map(|json| json.to_string())
        .unwrap_or_else(|_| body.to_string());
    let body_content = serialized.as_bytes();
    let pack_len = (16 + body_content.len()) as u32;
    let operation: u32 = match ops {
        Operation::AUTH => 7,
        Operation::HEARTBEAT => 2,
    };
    let mut res = Vec::with_capacity(pack_len as usize);
    res.extend_from_slice(&pack_len.to_be_bytes());
    res.extend_from_slice(&16_u16.to_be_bytes());
    res.extend_from_slice(&1_u16.to_be_bytes());
    res.extend_from_slice(&operation.to_be_bytes());
    res.extend_from_slice(&1_u32.to_be_bytes());
    res.extend_from_slice(body_content);
    res
}

pub fn get_msg_header(buf: &[u8]) -> MsgHead {
    let mut hdr = [0u8; 16];
    let len = buf.len().min(16);
    hdr[..len].copy_from_slice(&buf[..len]);
    MsgHead {
        pack_len: u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]),
        raw_header_size: u16::from_be_bytes([hdr[4], hdr[5]]),
        ver: u16::from_be_bytes([hdr[6], hdr[7]]),
        operation: u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]),
        seq_id: u32::from_be_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]),
    }
}

pub fn decompress(body: &[u8]) -> std::io::Result<Vec<u8>> {
    use brotlic::DecompressorReader;
    let mut decompressed_reader: DecompressorReader<&[u8]> = DecompressorReader::new(body);
    let mut decoded_input = Vec::new();
    let _ = decompressed_reader.read_to_end(&mut decoded_input)?;
    Ok(decoded_input)
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct FrameDecodeResult {
    pub auth_ack: bool,
    pub popularity: Option<u32>,
}

/// Panic-free Bilibili binary WebSocket packet decoder supporting plain JSON (`ver == 0`),
/// Zlib (`ver == 2`), and Brotli (`ver == 3` via `decompress`).
pub fn decode_bili_frames(buf: &[u8], out_jsons: &mut Vec<Value>) -> FrameDecodeResult {
    let mut result = FrameDecodeResult::default();
    let mut offset = 0usize;

    while offset + 16 <= buf.len() {
        let head = get_msg_header(&buf[offset..offset + 16]);
        let pack_len = head.pack_len as usize;
        let hdr_len = (head.raw_header_size as usize).max(16);
        if pack_len < hdr_len || offset + pack_len > buf.len() {
            break;
        }
        let body = &buf[offset + hdr_len..offset + pack_len];

        match head.operation {
            8 => result.auth_ack = true,
            3 if body.len() >= 4 => {
                result.popularity = Some(u32::from_be_bytes([body[0], body[1], body[2], body[3]]));
            }
            5 => match head.ver {
                2 | 3 => {
                    let decompressed = if head.ver == 3 {
                        decompress(body).ok()
                    } else {
                        let mut dec = Vec::new();
                        ZlibDecoder::new(body)
                            .read_to_end(&mut dec)
                            .ok()
                            .map(|_| dec)
                    };
                    if let Some(bytes) = decompressed {
                        let sub = decode_bili_frames(&bytes, out_jsons);
                        result.auth_ack |= sub.auth_ack;
                        if sub.popularity.is_some() {
                            result.popularity = sub.popularity;
                        }
                    }
                }
                0 => {
                    if let Ok(val) = serde_json::from_slice::<Value>(body) {
                        out_jsons.push(val);
                    }
                }
                other => {
                    log::debug!("Ignoring unknown Bilibili packet compression ver={}", other);
                }
            },
            _ => {}
        }

        offset += pack_len;
    }

    result
}

/// Map a Bilibili live JSON command packet into a lightweight [`BiliMessage`].
pub fn handle(json: Value) -> Option<BiliMessage> {
    let category = json["cmd"].as_str().unwrap_or("");
    let data = json.get("data").unwrap_or(&json);
    match category {
        "DANMU_MSG" => Some(BiliMessage::Danmu {
            user: json["info"][2][1]
                .as_str()
                .unwrap_or("<unknown>")
                .to_string(),
            text: json["info"][1].as_str().unwrap_or("").to_string(),
        }),
        "SEND_GIFT" => Some(BiliMessage::Gift {
            user: data["uname"]
                .as_str()
                .or_else(|| data["sender_uinfo"]["base"]["name"].as_str())
                .unwrap_or("<unknown>")
                .to_string(),
            gift: data["giftName"].as_str().unwrap_or("").to_string(),
            num: data["num"].as_u64().unwrap_or(1).to_string(),
        }),
        "INTERACT_WORD" if data["msg_type"].as_u64() == Some(1) => Some(BiliMessage::RoomEntry {
            user: data["uname"].as_str().unwrap_or("<unknown>").to_string(),
        }),
        "INTERACT_WORD_V2" => {
            let entry = data["pb"]
                .as_str()
                .and_then(|pb| general_purpose::STANDARD.decode(pb).ok())
                .and_then(|bytes| crate::pb::decode_interact_word(bytes.as_slice()));
            match entry {
                Some(entry) if entry.msg_type == 1 => {
                    let user = if !entry.username.trim().is_empty() {
                        entry.username
                    } else {
                        entry
                            .uinfo
                            .and_then(|u| u.base)
                            .map(|b| b.name)
                            .filter(|n| !n.trim().is_empty())
                            .unwrap_or_else(|| "<unknown>".to_string())
                    };
                    Some(BiliMessage::RoomEntry { user })
                }
                _ => Some(BiliMessage::Raw(json)),
            }
        }
        "ONLINE_RANK_COUNT" => Some(BiliMessage::OnlineRankCount {
            count: json["data"]["count"].as_u64().unwrap_or(0),
            online_count: json["data"]["online_count"].as_u64().unwrap_or(0),
        }),
        _ => Some(BiliMessage::Raw(json)),
    }
}

/// Validate explicit cookies and initialize the danmaku server info and auth message.
pub fn init_server_auto(
    provided_cookies: Option<&str>,
    room_id: &str,
) -> Result<(Value, AuthMessage), String> {
    let cookies = resolve_cookies(provided_cookies)?;
    log::info!("Using resolved cookies for authentication");
    try_init_server(&cookies, room_id)
}

/// Outcome of polling a managed [`LiveWsSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveWsPoll {
    /// Decoded one or more JSON command packets into `out_jsons` (or processed a control frame).
    Packets,
    /// Socket read timed out without new frames; caller may check cancellation and poll again.
    Timeout,
    /// Remote peer closed the WebSocket connection cleanly.
    Closed,
}

/// Managed blocking Bilibili live WebSocket session encapsulating `buvid3` fallback,
/// WBI `getDanmuInfo` handshake, `protover=3` AUTH, 30-second HEARTBEAT, and binary frame decoding.
pub struct LiveWsSession {
    ws: WebSocket<TlsStream<TcpStream>>,
    last_heartbeat: Instant,
    has_buvid: bool,
}

impl LiveWsSession {
    /// Connect and authenticate to `room_id` without a cancellation predicate.
    pub fn connect(
        room_id: u64,
        cookie_str: &str,
        buvid3: &str,
        read_timeout: Duration,
    ) -> Result<Self, String> {
        Self::connect_with_cancel(room_id, cookie_str, buvid3, read_timeout, || false)?
            .ok_or_else(|| "connection cancelled".to_string())
    }

    /// Connect and authenticate to `room_id`, checking `is_cancelled()` between blocking handshake steps.
    /// Returns `Ok(None)` when cancelled cleanly before completing the handshake.
    pub fn connect_with_cancel<F>(
        room_id: u64,
        cookie_str: &str,
        buvid3: &str,
        read_timeout: Duration,
        is_cancelled: F,
    ) -> Result<Option<Self>, String>
    where
        F: Fn() -> bool,
    {
        let room_id_str = room_id.to_string();
        let resolved_buvid = if buvid3.trim().is_empty() {
            try_init_buvid(Default::default())
                .ok()
                .map(|(_, b)| b.trim().to_string())
                .unwrap_or_default()
        } else {
            buvid3.trim().to_string()
        };
        let effective_cookie = append_cookie_kv(cookie_str, "buvid3", &resolved_buvid);
        if is_cancelled() {
            return Ok(None);
        }

        let (server_info, auth_msg) = try_init_server(&effective_cookie, &room_id_str)?;

        if is_cancelled() {
            return Ok(None);
        }

        let (mut ws, _resp) = connect_result(server_info["host_list"].clone())
            .map_err(|e| format!("WS connect failed: {}", e))?;

        if is_cancelled() {
            let _ = ws.close(None);
            return Ok(None);
        }

        if let Err(e) = ws.get_ref().get_ref().set_read_timeout(Some(read_timeout)) {
            log::warn!("Failed to set read timeout on Bilibili WS socket: {}", e);
        }

        let mut auth_payload = serde_json::json!({
            "uid": auth_msg.uid,
            "roomid": auth_msg.roomid,
            "protover": 3,
            "platform": "web",
            "type": 2,
            "key": auth_msg.key,
        });
        if !resolved_buvid.is_empty()
            && let Some(obj) = auth_payload.as_object_mut()
        {
            obj.insert("buvid".to_string(), Value::String(resolved_buvid.clone()));
        }
        let auth_json = auth_payload.to_string();

        ws.send(Message::Binary(make_packet(&auth_json, Operation::AUTH)))
            .map_err(|e| format!("Send AUTH failed: {}", e))?;

        ws.send(Message::Binary(make_packet("{}", Operation::HEARTBEAT)))
            .map_err(|e| format!("Send initial HEARTBEAT failed: {}", e))?;

        Ok(Some(Self {
            ws,
            last_heartbeat: Instant::now(),
            has_buvid: !resolved_buvid.is_empty(),
        }))
    }

    /// Whether a non-empty `buvid3` token was included in the WebSocket AUTH handshake.
    pub fn has_buvid(&self) -> bool {
        self.has_buvid
    }

    /// Maintain the 30-second heartbeat and read the next frame into `out_jsons`.
    pub fn poll_next(&mut self, out_jsons: &mut Vec<Value>) -> Result<LiveWsPoll, String> {
        out_jsons.clear();
        if self.last_heartbeat.elapsed() >= Duration::from_secs(30) {
            self.ws
                .send(Message::Binary(make_packet("{}", Operation::HEARTBEAT)))
                .map_err(|e| format!("Send HEARTBEAT failed: {}", e))?;
            self.last_heartbeat = Instant::now();
        }

        match self.ws.read() {
            Ok(Message::Binary(bytes)) => {
                let res = decode_bili_frames(&bytes, out_jsons);
                if res.auth_ack {
                    let _ = self
                        .ws
                        .send(Message::Binary(make_packet("{}", Operation::HEARTBEAT)));
                    self.last_heartbeat = Instant::now();
                }
                Ok(LiveWsPoll::Packets)
            }
            Ok(Message::Close(_)) => Ok(LiveWsPoll::Closed),
            Ok(_) => Ok(LiveWsPoll::Timeout),
            Err(tungstenite::Error::Io(ref e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                Ok(LiveWsPoll::Timeout)
            }
            Err(e) => Err(format!("WS read error: {}", e)),
        }
    }

    /// Gracefully close the underlying WebSocket stream.
    pub fn close(&mut self) {
        let _ = self.ws.close(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_channel::mpsc::channel;

    #[test]
    #[ignore = "requires live network access to Bilibili"]
    fn test_bili_live_client_connect() {
        // Get cookies from environment variable for real test
        let cookies =
            std::env::var("Cookie").unwrap_or_else(|_| "SESSDATA=dummy_sessdata".to_string());
        let room_id = "24779526";
        let (tx, _rx) = channel(10);
        let _client = BiliLiveClient::new(&cookies, room_id, tx);
    }

    #[test]
    fn test_make_packet_gen_damu_list_and_try_init_server_validation() {
        // Whitespace JSON must compute pack_len from actual serialized bytes, not input len
        let pkt = make_packet("{ \"uid\": 123 }", Operation::AUTH);
        let head = get_msg_header(&pkt[..16]);
        assert_eq!(head.pack_len as usize, pkt.len());
        assert_eq!(head.operation, 7);

        // Non-array or empty host_list gracefully falls back to default server without panicking
        let fallback = gen_damu_list(&serde_json::json!(null));
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].host, DanmuServer::default().host);

        // Invalid room_id / invalid header returns Err without panicking
        assert!(try_init_server("", "not-a-number").is_err());
        assert!(try_init_server("bad\ncookie", "24779526").is_err());
    }

    fn build_raw_frame(ver: u16, op: u32, body: &[u8]) -> Vec<u8> {
        let pack_len = (16 + body.len()) as u32;
        let mut out = Vec::with_capacity(pack_len as usize);
        out.extend_from_slice(&pack_len.to_be_bytes());
        out.extend_from_slice(&16u16.to_be_bytes());
        out.extend_from_slice(&ver.to_be_bytes());
        out.extend_from_slice(&op.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn test_decode_bili_frames_plain_zlib_and_truncated_safety() {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write;

        let auth_ack = build_raw_frame(1, 8, b"{}");
        let plain = build_raw_frame(0, 5, br#"{"cmd":"LIVE"}"#);

        let inner = build_raw_frame(0, 5, br#"{"cmd":"PREPARING"}"#);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&inner).unwrap();
        let zlib_body = encoder.finish().unwrap();
        let zlib_frame = build_raw_frame(2, 5, &zlib_body);

        let mut combined = Vec::new();
        combined.extend_from_slice(&auth_ack);
        combined.extend_from_slice(&plain);
        combined.extend_from_slice(&zlib_frame);

        let mut out = Vec::new();
        let res = decode_bili_frames(&combined, &mut out);
        assert!(res.auth_ack);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["cmd"], "LIVE");
        assert_eq!(out[1]["cmd"], "PREPARING");

        // Truncated/corrupt frame must never panic
        let mut out_bad = Vec::new();
        let _ = decode_bili_frames(&[0, 0, 0, 99, 0, 16, 0, 0, 0, 5, 0, 0, 0, 1], &mut out_bad);
        assert!(out_bad.is_empty());
    }
}
