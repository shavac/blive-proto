//! Async HTTP client for Bilibili live, authentication, room metadata, and danmaku send APIs.

use crate::auth::{
    BUVID_INIT_URL, DANMAKU_MAX_CHARS, QrPollStatus, ROOM_INIT_URL, SendDanmakuError, SessionData,
    UID_INIT_URL, form_urlencode, parse_live_user_info_response, parse_nav_response,
    parse_qr_poll_response, parse_send_danmaku_response,
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
use reqwest::header::{CONTENT_TYPE, COOKIE, HeaderMap, ORIGIN, REFERER, SET_COOKIE};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const API_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub const NAV_URL: &str = UID_INIT_URL;
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
pub const GET_INFO_BY_ROOM_URL: &str = ROOM_INIT_URL;
pub const ROOM_INIT_API_URL: &str = "https://api.live.bilibili.com/room/v1/Room/room_init";
pub const FINGER_SPI_URL: &str = "https://api.bilibili.com/x/frontend/finger/spi";
pub const BUVID_FALLBACK_URL: &str = BUVID_INIT_URL;
pub const ACTIVE_SUPER_CHAT_URL: &str =
    "https://api.live.bilibili.com/av/v1/SuperChat/getMessageList";

const LIVE_REFERER: &str = "https://live.bilibili.com/";

fn collect_set_cookies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|hv| hv.to_str().ok().map(str::to_string))
        .collect()
}

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

    fn live_get(&self, url: &str, cookie: Option<&str>) -> reqwest::RequestBuilder {
        let req = self.http.get(url).header(REFERER, LIVE_REFERER);
        match cookie.map(str::trim).filter(|s| !s.is_empty()) {
            Some(c) => req.header(COOKIE, c),
            None => req,
        }
    }

    async fn fetch_auth_endpoint(
        &self,
        url: &str,
        referer: &str,
        cookie: &str,
        label: &str,
        fallback_mid: u64,
        parser: fn(&Value, u64) -> Option<AuthState>,
    ) -> Result<AuthState, String> {
        let resp = self
            .http
            .get(url)
            .header(COOKIE, cookie)
            .header(REFERER, referer)
            .send()
            .await
            .map_err(|e| format!("Failed to request Bilibili {} API: {}", label, e))?;
        let json: Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to decode Bilibili {} JSON: {}", label, e))?;
        parser(&json, fallback_mid).ok_or_else(|| {
            format!(
                "unexpected {} payload (code={})",
                label,
                json["code"].as_i64().unwrap_or(-1)
            )
        })
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
            .fetch_auth_endpoint(
                NAV_URL,
                "https://www.bilibili.com/",
                &cookie,
                "nav",
                fallback_mid,
                parse_nav_response,
            )
            .await
        {
            Ok(state) => return Ok(state),
            Err(e) => e,
        };

        log::warn!(
            "Primary Bilibili nav API unavailable ({}), falling back to live user info API",
            nav_err
        );

        self.fetch_auth_endpoint(
            LIVE_USER_INFO_URL,
            LIVE_REFERER,
            &cookie,
            "live user_info",
            fallback_mid,
            parse_live_user_info_response,
        )
        .await
        .map_err(|live_err| format!("nav: {}; live_user_info: {}", nav_err, live_err))
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

        let set_cookies = collect_set_cookies(response.headers());
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to decode Bilibili QR poll JSON: {}", e))?;

        let data = &body["data"];
        Ok(parse_qr_poll_response(
            data["code"].as_i64().unwrap_or(-1),
            data["message"].as_str().unwrap_or(""),
            &set_cookies,
            data["url"].as_str(),
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

        let cookie = session.to_cookie_header();
        let resp: Value = self
            .live_get(ROOM_INFO_OLD_URL, Some(&cookie))
            .query(&[("mid", mid)])
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

        if let Ok(resp) = self
            .live_get(FOLLOWING_FEED_LIST_URL, Some(&cookie))
            .query(&[("page", "1"), ("pagesize", "30")])
            .send()
            .await
            && let Ok(json) = resp.json::<Value>().await
            && let Some(items) = parse_following_feed_list_response(&json)
        {
            return Ok(items);
        }

        let fallback_resp: Value = self
            .live_get(FOLLOWING_LIVE_URL, Some(&cookie))
            .query(&[("page", "1"), ("page_size", "29")])
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
        if uid > 0
            && let Ok(resp) = self
                .live_get(MASTER_INFO_URL, cookie)
                .query(&[("uid", uid.to_string())])
                .send()
                .await
            && let Ok(json) = resp.json::<Value>().await
            && let Some(uname) = parse_master_info_uname(&json)
        {
            return uname;
        }

        if let Ok(resp) = self
            .live_get(GET_INFO_BY_ROOM_URL, cookie)
            .query(&[("room_id", room_id.to_string())])
            .send()
            .await
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
        let room_str = input_room_id.to_string();

        if let Ok(resp) = self
            .live_get(ROOM_GET_INFO_URL, cookie)
            .query(&[("room_id", &room_str)])
            .send()
            .await
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

        if let Ok(resp) = self
            .live_get(GET_INFO_BY_ROOM_URL, cookie)
            .query(&[("room_id", &room_str)])
            .send()
            .await
            && let Ok(json) = resp.json::<Value>().await
            && let Some(snap) = parse_get_info_by_room_json(&json, input_room_id)
        {
            return Ok(snap);
        }

        let init_json: Value = self
            .live_get(ROOM_INIT_API_URL, cookie)
            .query(&[("id", &room_str)])
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

        if let Ok(resp) = self
            .live_get(FINGER_SPI_URL, Some(cookie_str))
            .timeout(Duration::from_secs(3))
            .send()
            .await
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
            && let Some(val) = parse_set_cookie_buvid3(&collect_set_cookies(resp.headers()))
        {
            return val;
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
        let req = self
            .live_get(ACTIVE_SUPER_CHAT_URL, cookie)
            .query(&[("room_id", room_id.to_string())])
            .timeout(Duration::from_secs(3));
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
