// src/api.rs
//! Async HTTP client for Bilibili live, authentication, room metadata, and danmaku send APIs.

use crate::auth::{
    DANMAKU_MAX_CHARS, QrPollStatus, SendDanmakuError, SessionData, form_urlencode,
    parse_live_user_info_response, parse_nav_response, parse_qr_poll_response,
    parse_send_danmaku_response,
};
use crate::commands::{
    extract_cookie_value, parse_active_super_chats_json, parse_buvid3_spi_json,
    parse_following_feed_list_response, parse_following_live_response, parse_get_info_by_room_json,
    parse_master_info_uname, parse_my_room_response, parse_qr_generate_response,
    parse_room_get_info_json, parse_room_init_json, parse_set_cookie_buvid3,
};
use crate::models::{
    AuthState, QrCodeInfo, RawBiliEvent, RoomInfoSnapshot, RoomQuickItem, now_unix_ms,
};
use reqwest::header::{CONTENT_TYPE, COOKIE, ORIGIN, REFERER, SET_COOKIE};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const API_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub const NAV_URL: &str = "https://api.bilibili.com/x/web-interface/nav";
pub const LIVE_USER_INFO_URL: &str =
    "https://api.live.bilibili.com/xlive/web-ucenter/user/get_user_info";
pub const QR_GENERATE_URL: &str =
    "https://passport.bilibili.com/x/passport-login/web/qrcode/generate";
pub const QR_POLL_URL: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/poll";
pub const MSG_SEND_URL: &str = "https://api.live.bilibili.com/msg/send";
pub const ROOM_INFO_OLD_URL: &str = "https://api.live.bilibili.com/room/v1/Room/getRoomInfoOld";
pub const FOLLOWING_FEED_LIST_URL: &str =
    "https://api.live.bilibili.com/relation/v1/feed/feed_list";
pub const FOLLOWING_LIVE_URL: &str =
    "https://api.live.bilibili.com/xlive/web-ucenter/user/following";
pub const MASTER_INFO_URL: &str = "https://api.live.bilibili.com/live_user/v1/Master/info";
pub const ROOM_GET_INFO_URL: &str = "https://api.live.bilibili.com/room/v1/Room/get_info";
pub const GET_INFO_BY_ROOM_URL: &str =
    "https://api.live.bilibili.com/xlive/web-room/v1/index/getInfoByRoom";
pub const ROOM_INIT_API_URL: &str = "https://api.live.bilibili.com/room/v1/Room/room_init";
pub const FINGER_SPI_URL: &str = "https://api.bilibili.com/x/frontend/finger/spi";
pub const BUVID_FALLBACK_URL: &str = "https://data.bilibili.com/v/";
pub const ACTIVE_SUPER_CHAT_URL: &str =
    "https://api.live.bilibili.com/av/v1/SuperChat/getMessageList";

/// Async HTTP client encapsulating all Bilibili Web / Live REST endpoints and response parsers.
#[derive(Clone)]
pub struct BiliApiClient {
    http: reqwest::Client,
}

impl Default for BiliApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl BiliApiClient {
    /// Create a client configured with the standard desktop User-Agent and a 15-second timeout.
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(API_USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()
            .expect("Failed to construct BiliApiClient HTTP client");
        Self { http }
    }

    /// Send a live danmaku text message to `room_id` via `POST /msg/send`.
    pub async fn send_danmaku(
        &self,
        session: &SessionData,
        room_id: u64,
        msg: &str,
    ) -> Result<(), SendDanmakuError> {
        if room_id == 0 {
            return Err(SendDanmakuError::NotConnected);
        }
        if !session.is_valid_format() {
            return Err(SendDanmakuError::NotLoggedIn);
        }

        let trimmed = msg.trim();
        if trimmed.is_empty() {
            return Err(SendDanmakuError::EmptyMessage);
        }
        if trimmed.chars().count() > DANMAKU_MAX_CHARS {
            return Err(SendDanmakuError::MessageTooLong {
                max: DANMAKU_MAX_CHARS,
            });
        }

        let rnd = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .to_string();
        let room_str = room_id.to_string();
        let csrf = session.bili_jct.trim();
        let body = form_urlencode(&[
            ("bubble", "0"),
            ("msg", trimmed),
            ("color", "16777215"),
            ("mode", "1"),
            ("fontsize", "25"),
            ("rnd", &rnd),
            ("roomid", &room_str),
            ("csrf", csrf),
            ("csrf_token", csrf),
        ]);

        let resp = self
            .http
            .post(MSG_SEND_URL)
            .header(COOKIE, session.to_cookie_header())
            .header(REFERER, format!("https://live.bilibili.com/{}", room_id))
            .header(ORIGIN, "https://live.bilibili.com")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| SendDanmakuError::Network(e.to_string()))?;

        let json: Value = resp
            .json()
            .await
            .map_err(|e| SendDanmakuError::Network(e.to_string()))?;

        parse_send_danmaku_response(&json)
    }

    /// Validate cookies against `x/web-interface/nav`, falling back to `xlive/web-ucenter/user/get_user_info`.
    pub async fn validate_session(&self, session: &SessionData) -> Result<AuthState, String> {
        if !session.is_valid_format() {
            return Ok(AuthState::default());
        }

        let cookie = session.to_cookie_header();
        let fallback_mid = session.dede_user_id.parse().unwrap_or(0);

        let nav_err = match self
            .http
            .get(NAV_URL)
            .header(COOKIE, &cookie)
            .header(REFERER, "https://www.bilibili.com/")
            .send()
            .await
        {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(json) => {
                    if let Some(state) = parse_nav_response(&json, fallback_mid) {
                        return Ok(state);
                    }
                    format!(
                        "unexpected nav payload (code={})",
                        json["code"].as_i64().unwrap_or(-1)
                    )
                }
                Err(e) => format!("Failed to decode Bilibili nav JSON: {}", e),
            },
            Err(e) => format!("Failed to request Bilibili nav API: {}", e),
        };

        log::warn!(
            "Primary Bilibili nav API unavailable ({}), falling back to live user info API",
            nav_err
        );

        let live_err = match self
            .http
            .get(LIVE_USER_INFO_URL)
            .header(COOKIE, &cookie)
            .header(REFERER, "https://live.bilibili.com/")
            .send()
            .await
        {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(json) => {
                    if let Some(state) = parse_live_user_info_response(&json, fallback_mid) {
                        return Ok(state);
                    }
                    format!(
                        "unexpected live user_info payload (code={})",
                        json["code"].as_i64().unwrap_or(-1)
                    )
                }
                Err(e) => format!("Failed to decode Bilibili live user_info JSON: {}", e),
            },
            Err(e) => format!("Failed to request Bilibili live user_info API: {}", e),
        };

        Err(format!("nav: {}; live_user_info: {}", nav_err, live_err))
    }

    /// Request a fresh QR login URL and key from `passport.bilibili.com`.
    pub async fn generate_qr(&self) -> Result<QrCodeInfo, String> {
        let resp: Value = self
            .http
            .get(QR_GENERATE_URL)
            .header(REFERER, "https://passport.bilibili.com/login")
            .send()
            .await
            .map_err(|e| format!("Failed to request Bilibili QR generate API: {}", e))?
            .json()
            .await
            .map_err(|e| format!("Failed to decode Bilibili QR generate JSON: {}", e))?;

        parse_qr_generate_response(&resp)
    }

    /// Poll QR scan confirmation status.
    pub async fn poll_qr(&self, qrcode_key: &str) -> Result<QrPollStatus, String> {
        let response = self
            .http
            .get(QR_POLL_URL)
            .query(&[("qrcode_key", qrcode_key)])
            .header(REFERER, "https://passport.bilibili.com/login")
            .send()
            .await
            .map_err(|e| format!("Failed to request Bilibili QR poll API: {}", e))?;

        let set_cookies: Vec<String> = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|hv| hv.to_str().ok().map(str::to_string))
            .collect();

        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to decode Bilibili QR poll JSON: {}", e))?;

        let code = body["data"]["code"].as_i64().unwrap_or(-1);
        let message = body["data"]["message"].as_str().unwrap_or("");
        let fallback_url = body["data"]["url"].as_str();

        Ok(parse_qr_poll_response(
            code,
            message,
            &set_cookies,
            fallback_url,
        ))
    }

    /// Fetch the logged-in user's own live room via `Room/getRoomInfoOld`.
    pub async fn fetch_my_room(
        &self,
        session: &SessionData,
        room_uname: &str,
    ) -> Result<Option<RoomQuickItem>, String> {
        let mid = session.dede_user_id.trim();
        if mid.is_empty() {
            return Ok(None);
        }

        let resp: Value = self
            .http
            .get(ROOM_INFO_OLD_URL)
            .query(&[("mid", mid)])
            .header(COOKIE, session.to_cookie_header())
            .header(REFERER, "https://live.bilibili.com/")
            .send()
            .await
            .map_err(|e| format!("Failed to query getRoomInfoOld: {}", e))?
            .json()
            .await
            .map_err(|e| format!("Failed to parse getRoomInfoOld JSON: {}", e))?;

        Ok(parse_my_room_response(&resp, room_uname))
    }

    /// Fetch followed streamers that are currently live.
    pub async fn fetch_following_live(
        &self,
        session: &SessionData,
    ) -> Result<Vec<RoomQuickItem>, String> {
        let cookie = session.to_cookie_header();

        let primary = self
            .http
            .get(FOLLOWING_FEED_LIST_URL)
            .query(&[("page", "1"), ("pagesize", "30")])
            .header(COOKIE, &cookie)
            .header(REFERER, "https://live.bilibili.com/")
            .send()
            .await;

        if let Ok(resp) = primary
            && let Ok(json) = resp.json::<Value>().await
            && let Some(items) = parse_following_feed_list_response(&json)
        {
            return Ok(items);
        }

        let fallback_resp: Value = self
            .http
            .get(FOLLOWING_LIVE_URL)
            .query(&[("page", "1"), ("page_size", "29")])
            .header(COOKIE, &cookie)
            .header(REFERER, "https://live.bilibili.com/")
            .send()
            .await
            .map_err(|e| format!("Failed to query Bilibili following live list: {}", e))?
            .json()
            .await
            .map_err(|e| format!("Failed to parse following live JSON: {}", e))?;

        Ok(parse_following_live_response(&fallback_resp))
    }

    /// Resolve anchor username from `Master/info` with `getInfoByRoom` fallback.
    pub async fn fetch_anchor_uname(&self, uid: u64, room_id: u64, cookie: Option<&str>) -> String {
        if uid > 0 {
            let mut req = self
                .http
                .get(MASTER_INFO_URL)
                .query(&[("uid", uid.to_string())])
                .header(REFERER, "https://live.bilibili.com/");
            if let Some(c) = cookie.filter(|s| !s.trim().is_empty()) {
                req = req.header(COOKIE, c);
            }
            if let Ok(resp) = req.send().await
                && let Ok(json) = resp.json::<Value>().await
                && let Some(uname) = parse_master_info_uname(&json)
            {
                return uname;
            }
        }

        let mut fallback_req = self
            .http
            .get(GET_INFO_BY_ROOM_URL)
            .query(&[("room_id", room_id.to_string())])
            .header(REFERER, "https://live.bilibili.com/");
        if let Some(c) = cookie.filter(|s| !s.trim().is_empty()) {
            fallback_req = fallback_req.header(COOKIE, c);
        }
        if let Ok(resp) = fallback_req.send().await
            && let Ok(json) = resp.json::<Value>().await
            && let Some(snap) = parse_get_info_by_room_json(&json, room_id)
            && !snap.uname.is_empty()
        {
            return snap.uname;
        }

        String::new()
    }

    /// Resolve room metadata via `Room/get_info` + anchor `uname`, with `getInfoByRoom` and `room_init` fallbacks.
    pub async fn fetch_room_meta(
        &self,
        input_room_id: u64,
        cookie: Option<&str>,
    ) -> Result<RoomInfoSnapshot, String> {
        let mut info_req = self
            .http
            .get(ROOM_GET_INFO_URL)
            .query(&[("room_id", input_room_id.to_string())])
            .header(REFERER, "https://live.bilibili.com/");
        if let Some(c) = cookie.filter(|s| !s.trim().is_empty()) {
            info_req = info_req.header(COOKIE, c);
        }

        if let Ok(resp) = info_req.send().await
            && let Ok(json) = resp.json::<Value>().await
            && let Some(mut snap) = parse_room_get_info_json(&json, None)
        {
            if snap.uname.is_empty() {
                snap.uname = self
                    .fetch_anchor_uname(snap.uid, snap.room_id, cookie)
                    .await;
            }
            return Ok(snap);
        }

        let mut req = self
            .http
            .get(GET_INFO_BY_ROOM_URL)
            .query(&[("room_id", input_room_id.to_string())])
            .header(REFERER, "https://live.bilibili.com/");
        if let Some(c) = cookie.filter(|s| !s.trim().is_empty()) {
            req = req.header(COOKIE, c);
        }

        if let Ok(resp) = req.send().await
            && let Ok(json) = resp.json::<Value>().await
            && let Some(snap) = parse_get_info_by_room_json(&json, input_room_id)
        {
            return Ok(snap);
        }

        let mut init_req = self
            .http
            .get(ROOM_INIT_API_URL)
            .query(&[("id", input_room_id.to_string())])
            .header(REFERER, "https://live.bilibili.com/");
        if let Some(c) = cookie.filter(|s| !s.trim().is_empty()) {
            init_req = init_req.header(COOKIE, c);
        }

        let init_json: Value = init_req
            .send()
            .await
            .map_err(|e| format!("Failed to query Bilibili room_init API: {}", e))?
            .json()
            .await
            .map_err(|e| format!("Failed to decode Bilibili room_init JSON: {}", e))?;

        parse_room_init_json(&init_json, input_room_id)
    }

    /// Resolve `buvid3` from existing cookies, `x/frontend/finger/spi`, or `data.bilibili.com/v/` `Set-Cookie`.
    pub async fn fetch_buvid3(&self, cookie_str: &str) -> String {
        if let Some(existing) = extract_cookie_value(cookie_str, "buvid3") {
            return existing;
        }

        let mut spi_req = self
            .http
            .get(FINGER_SPI_URL)
            .timeout(Duration::from_secs(3))
            .header(REFERER, "https://live.bilibili.com/");
        if !cookie_str.trim().is_empty() {
            spi_req = spi_req.header(COOKIE, cookie_str);
        }
        if let Ok(resp) = spi_req.send().await
            && let Ok(json) = resp.json::<Value>().await
            && let Some(b3) = parse_buvid3_spi_json(&json)
        {
            return b3;
        }

        if let Ok(resp) = self
            .http
            .get(BUVID_FALLBACK_URL)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            let set_cookies: Vec<String> = resp
                .headers()
                .get_all(SET_COOKIE)
                .iter()
                .filter_map(|hv| hv.to_str().ok().map(str::to_string))
                .collect();
            if let Some(val) = parse_set_cookie_buvid3(&set_cookies) {
                return val;
            }
        }

        String::new()
    }

    /// Fetch currently active SuperChats via `av/v1/SuperChat/getMessageList`.
    pub async fn fetch_active_super_chats(
        &self,
        room_id: u64,
        room_uname: &str,
        cookie: Option<&str>,
    ) -> Vec<RawBiliEvent> {
        let mut req = self
            .http
            .get(ACTIVE_SUPER_CHAT_URL)
            .query(&[("room_id", room_id.to_string())])
            .timeout(Duration::from_secs(3))
            .header(REFERER, "https://live.bilibili.com/");
        if let Some(c) = cookie.filter(|s| !s.trim().is_empty()) {
            req = req.header(COOKIE, c);
        }
        match req.send().await {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(json) => {
                    parse_active_super_chats_json(&json, room_id, room_uname, now_unix_ms())
                }
                Err(e) => {
                    log::debug!(
                        "Failed to decode active SC list for room {}: {}",
                        room_id,
                        e
                    );
                    Vec::new()
                }
            },
            Err(e) => {
                log::debug!("Failed to fetch active SC list for room {}: {}", room_id, e);
                Vec::new()
            }
        }
    }
}
