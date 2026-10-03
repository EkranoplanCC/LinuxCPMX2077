//! Nexus Mods single sign-on. The user logs in on nexusmods.com in their own
//! browser and approves this app; Nexus then hands the app an API key over a
//! websocket. The app never sees the user's password.
//!
//! Flow (https://github.com/Nexus-Mods/sso-integration-demo):
//! 1. connect to `wss://sso.nexusmods.com`
//! 2. send `{"id": <uuid>, "token": null, "protocol": 2}`
//! 3. open `https://www.nexusmods.com/sso?id=<uuid>&application=<slug>`
//! 4. receive `{"success": true, "data": {"api_key": "..."}}`
//!
//! Nexus only accepts application slugs it has issued to a registered mod
//! manager, so the slug is configuration, not something we can invent.

use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::{Error, Result};

pub const SSO_SOCKET: &str = "wss://sso.nexusmods.com";
pub const SSO_PAGE: &str = "https://www.nexusmods.com/sso";
/// Slug baked in at build time, e.g. `NEXUS_SSO_APP_SLUG=myapp cargo build`.
pub const BUILT_IN_SLUG: Option<&str> = option_env!("NEXUS_SSO_APP_SLUG");
const TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Deserialize)]
struct Reply {
    success: bool,
    #[serde(default)]
    data: Option<ReplyData>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReplyData {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    connection_token: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Event {
    Connected,
    ApiKey(String),
}

fn parse_reply(text: &str) -> Result<Option<Event>> {
    let r: Reply = serde_json::from_str(text)?;
    if !r.success {
        return Err(Error::Nexus(format!("sign-in refused: {}", r.error.unwrap_or_else(|| "unknown error".into()))));
    }
    let Some(d) = r.data else { return Ok(None) };
    if let Some(k) = d.api_key.filter(|k| !k.trim().is_empty()) {
        return Ok(Some(Event::ApiKey(k)));
    }
    Ok(d.connection_token.map(|_| Event::Connected))
}

pub fn authorize_url(id: &str, slug: &str) -> String {
    let mut u = url::Url::parse(SSO_PAGE).expect("static url");
    u.query_pairs_mut().append_pair("id", id).append_pair("application", slug);
    u.into()
}

fn set_read_timeout(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>, d: Duration) {
    let tcp = match ws.get_mut() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => s.get_mut(),
        _ => return,
    };
    let _ = tcp.set_read_timeout(Some(d));
}

/// Run the SSO handshake. `open_browser` is called with the URL the user
/// must visit; `cancel` aborts the wait. Returns the API key.
pub fn login(slug: &str, open_browser: impl FnOnce(&str) -> Result<()>, cancel: Arc<AtomicBool>) -> Result<String> {
    if slug.trim().is_empty() {
        return Err(Error::Nexus("Nexus sign-in needs an application slug issued by Nexus Mods".into()));
    }
    let (mut ws, _) = tungstenite::connect(SSO_SOCKET).map_err(|e| Error::Nexus(format!("SSO connect: {e}")))?;
    set_read_timeout(&mut ws, Duration::from_secs(1));

    let id = uuid::Uuid::new_v4().to_string();
    let hello = serde_json::json!({ "id": id, "token": null, "protocol": 2 });
    ws.send(Message::Text(hello.to_string().into())).map_err(|e| Error::Nexus(format!("SSO send: {e}")))?;

    let started = Instant::now();
    let mut browser = Some(open_browser);
    let mut last_ping = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = ws.close(None);
            return Err(Error::Nexus("sign-in cancelled".into()));
        }
        if started.elapsed() > TIMEOUT {
            let _ = ws.close(None);
            return Err(Error::Nexus("sign-in timed out".into()));
        }
        // Keep the socket alive while the user is in the browser.
        if last_ping.elapsed() > Duration::from_secs(30) {
            let _ = ws.send(Message::Ping(Vec::new().into()));
            last_ping = Instant::now();
        }
        match ws.read() {
            Ok(Message::Text(t)) => match parse_reply(t.as_str())? {
                Some(Event::Connected) => {
                    if let Some(open) = browser.take() {
                        open(&authorize_url(&id, slug))?;
                    }
                }
                Some(Event::ApiKey(k)) => {
                    let _ = ws.close(None);
                    return Ok(k);
                }
                None => {}
            },
            Ok(Message::Close(_)) => return Err(Error::Nexus("Nexus closed the sign-in connection".into())),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(Error::Nexus(format!("SSO: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sso_messages() {
        assert_eq!(parse_reply(r#"{"success":true,"data":{"connection_token":"abc"},"error":null}"#).unwrap(), Some(Event::Connected));
        assert_eq!(
            parse_reply(r#"{"success":true,"data":{"api_key":"KEY"},"error":null}"#).unwrap(),
            Some(Event::ApiKey("KEY".into()))
        );
        assert!(parse_reply(r#"{"success":false,"data":null,"error":"Invalid application"}"#).is_err());
    }

    #[test]
    fn builds_authorize_url() {
        assert_eq!(
            authorize_url("1234-abcd", "my app"),
            "https://www.nexusmods.com/sso?id=1234-abcd&application=my+app"
        );
    }
}
