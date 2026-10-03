//! Test helpers shared by the API client tests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// A canned response: (path substring, status, extra headers, body).
pub type Canned = (&'static str, u16, Vec<(&'static str, String)>, String);

#[derive(Debug, Clone)]
pub struct Seen {
    pub line: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Minimal HTTP/1.1 server answering from recorded fixtures. `{addr}` in a
/// response body or header is replaced with the server's own address.
pub fn serve(responses: Vec<Canned>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let responses = Arc::new(Mutex::new(responses));
    let self_addr = addr.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut headers = Vec::new();
            let mut len = 0;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                let (k, v) = h.split_once(':').unwrap();
                let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                if k == "content-length" {
                    len = v.parse().unwrap();
                }
                headers.push((k, v));
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            log.lock().unwrap().push(Seen { line: line.trim().to_string(), headers, body: body.clone() });
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut rs = responses.lock().unwrap();
            // First matching response is used once, unless it's the last match.
            let idx = rs.iter().position(|(p, ..)| path.contains(p));
            let (status, extra, out) = match idx {
                Some(i) => {
                    let matches = rs.iter().filter(|(p, ..)| path.contains(p)).count();
                    let r = if matches > 1 { rs.remove(i) } else { rs[i].clone() };
                    (r.1, r.2, r.3)
                }
                None => (404, vec![], r#"{"message":"No route"}"#.to_string()),
            };
            let out = out.replace("{addr}", &self_addr);
            let extra: Vec<(&str, String)> = extra.into_iter().map(|(k, v)| (k, v.replace("{addr}", &self_addr))).collect();
            let mut resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                out.len()
            );
            for (k, v) in extra {
                resp.push_str(&format!("{k}: {v}\r\n"));
            }
            resp.push_str("\r\n");
            resp.push_str(&out);
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    (addr, seen)
}
