//! Read-only Model Context Protocol server, so an external agent (e.g.
//! Claude) can look at the mod library, the compatibility index and the
//! game's logs.
//!
//! Run with `cp2077-modmanager --mcp`; it speaks JSON-RPC over stdin/stdout.
//! Everything is read-only by construction: the database is opened with
//! SQLite's read-only flag plus `query_only`, there are no tools that write
//! or run anything, and logs can only be read from a fixed list of known
//! locations, never from a caller-supplied path.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::analysis::{self, Kind};
use crate::crash;
use crate::db::Db;
use crate::{APP_NAME, APP_VERSION, Error, Result};

const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_LOG_LINES: usize = 2000;

pub fn default_db_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_NAME).join("library.sqlite3"))
}

pub struct Server {
    db: Db,
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required, "additionalProperties": false },
        "annotations": { "readOnlyHint": true, "openWorldHint": false },
    })
}

fn tools() -> Value {
    let kinds: Vec<&str> = Kind::ALL.iter().map(|k| k.as_str()).collect();
    json!([
        tool("list_games", "Cyberpunk 2077 installs known to the mod manager, with store, path, Steam build and executable version.", json!({}), &[]),
        tool("list_mods", "Mods installed in a game: name, version, source (manual/nexus), Nexus ids, file count, the game build they were installed on, and status ('installed' means enabled, 'disabled' means its files are out of the game).",
            json!({ "game_id": { "type": "integer" } }), &["game_id"]),
        tool("get_mod", "One mod in detail: its files (game-relative path, size, sha256) and everything the compatibility index recorded it touching.",
            json!({ "mod_id": { "type": "integer" } }), &["mod_id"]),
        tool("find_touches", "Which installed mods touch a game element. `key` is matched as a case-insensitive substring (e.g. 'PlayerPuppet', 'Items.Preset_', a resource hash).",
            json!({
                "key": { "type": "string" },
                "kind": { "type": "string", "enum": kinds },
                "game_id": { "type": "integer" }
            }), &["key"]),
        tool("compatibility_report", "Conflicts between enabled mods (same method replaced, same resources, same tweak values, duplicate plugins) and frameworks mods need but are missing, with a per-mod summary.",
            json!({ "game_id": { "type": "integer" } }), &["game_id"]),
        tool("crash_analysis", "Errors and warnings from the game's crash reports and the framework logs, each matched to the installed mods it mentions and marked as from the last game session or earlier, plus the mods most often implicated, a step-by-step startup timeline (first failed step marked) and known problem mods from the modding wiki.",
            json!({ "game_id": { "type": "integer" } }), &["game_id"]),
        tool("list_logs", "Log files the game and modding frameworks wrote (CET, RED4ext and its plugins, redscript, CET mods, crash reports in the Proton prefix, Proton's log), with sizes and modification times.",
            json!({ "game_id": { "type": "integer" } }), &["game_id"]),
        tool("read_log", "The last lines of one log from list_logs, identified by its `name`.",
            json!({
                "game_id": { "type": "integer" },
                "name": { "type": "string" },
                "max_lines": { "type": "integer", "minimum": 1, "maximum": MAX_LOG_LINES }
            }), &["game_id", "name"]),
    ])
}

fn arg_i64(args: &Value, name: &str) -> Result<i64> {
    args.get(name).and_then(Value::as_i64).ok_or_else(|| Error::Other(format!("missing integer argument `{name}`")))
}

fn arg_str<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name).and_then(Value::as_str).ok_or_else(|| Error::Other(format!("missing string argument `{name}`")))
}

impl Server {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn open_default() -> Result<Self> {
        let path = default_db_path().ok_or_else(|| Error::Other("no XDG data dir".into()))?;
        if !path.is_file() {
            return Err(Error::Other(format!("no mod library at {} yet; open the mod manager once first", path.display())));
        }
        Ok(Self::new(Db::open_read_only(&path)?))
    }

    fn call_tool(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "list_games" => Ok(serde_json::to_value(self.db.games()?)?),
            "list_mods" => Ok(serde_json::to_value(self.db.mods(arg_i64(args, "game_id")?)?)?),
            "get_mod" => {
                let id = arg_i64(args, "mod_id")?;
                let m = self.db.get_mod(id)?;
                let files: Vec<Value> = self
                    .db
                    .mod_files(id)?
                    .into_iter()
                    .map(|f| json!({ "path": f.rel_path, "size": f.size, "sha256": f.sha256 }))
                    .collect();
                let touches = self.db.touches(id)?;
                let resources: Vec<&str> =
                    touches.iter().filter(|t| t.kind == Kind::Resource).map(|t| t.key.as_str()).collect();
                let other: Vec<&analysis::Touch> = touches.iter().filter(|t| t.kind != Kind::Resource).collect();
                Ok(json!({
                    "mod": m,
                    "files": files,
                    "resources": { "count": resources.len(), "sample_hashes": resources.iter().take(50).collect::<Vec<_>>() },
                    "touches": other,
                    "indexed": self.db.index_version(id)?.is_some(),
                }))
            }
            "find_touches" => {
                let key = arg_str(args, "key")?.to_lowercase();
                let kind = args.get("kind").and_then(Value::as_str).and_then(Kind::parse);
                let game = args.get("game_id").and_then(Value::as_i64);
                let mut hits = Vec::new();
                for g in self.db.games()? {
                    if game.is_some_and(|id| id != g.id) {
                        continue;
                    }
                    for m in self.db.mods(g.id)? {
                        for t in self.db.touches(m.id)? {
                            if kind.is_some_and(|k| k != t.kind) || !t.key.to_lowercase().contains(&key) {
                                continue;
                            }
                            hits.push(json!({ "game_id": g.id, "mod_id": m.id, "mod": m.name, "enabled": m.enabled(), "kind": t.kind, "key": t.key, "file": t.file }));
                            if hits.len() >= 500 {
                                return Ok(json!({ "results": hits, "truncated": true }));
                            }
                        }
                    }
                }
                Ok(json!({ "results": hits, "truncated": false }))
            }
            "compatibility_report" => {
                let game = self.db.game(arg_i64(args, "game_id")?)?;
                let (report, unindexed) = analysis::report_from_index(&self.db, &game)?;
                Ok(json!({ "report": report, "unindexed_mods": unindexed }))
            }
            "list_logs" => {
                let game = self.db.game(arg_i64(args, "game_id")?)?;
                Ok(serde_json::to_value(crash::known_logs(Path::new(&game.path)))?)
            }
            "read_log" => {
                let game = self.db.game(arg_i64(args, "game_id")?)?;
                let name = arg_str(args, "name")?;
                let max = args.get("max_lines").and_then(Value::as_u64).unwrap_or(200).clamp(1, MAX_LOG_LINES as u64) as usize;
                // Only names that list_logs would return are readable.
                let log = crash::find_log(Path::new(&game.path), name)
                    .map_err(|_| Error::Other(format!("unknown log `{name}`; call list_logs first")))?;
                Ok(json!({ "name": log.name, "text": crash::tail_lines(&log.path, max)? }))
            }
            "crash_analysis" => {
                let game = self.db.game(arg_i64(args, "game_id")?)?;
                Ok(serde_json::to_value(crash::analyze(&self.db, &game)?)?)
            }
            other => Err(Error::Other(format!("unknown tool `{other}`"))),
        }
    }

    /// Handle one JSON-RPC message; `None` for notifications.
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id")?.clone();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
                Ok(json!({
                    "protocolVersion": requested,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": APP_NAME, "version": APP_VERSION },
                    "instructions": "Read-only view of a Cyberpunk 2077 mod library on Linux/Proton. Start with list_games, then list_mods and compatibility_report. Use find_touches to see which mods change a class, record or resource, and list_logs/read_log after a crash.",
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                // Tool failures are reported to the model, not as protocol errors.
                Ok(match self.call_tool(name, &args) {
                    Ok(v) => json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }],
                        "structuredContent": if v.is_object() { v } else { json!({ "result": v }) },
                    }),
                    Err(e) => json!({ "content": [{ "type": "text", "text": e.to_string() }], "isError": true }),
                })
            }
            _ => Err(json!({ "code": -32601, "message": format!("method not found: {method}") })),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        })
    }

    /// Serve newline-delimited JSON-RPC until the input closes.
    pub fn serve(&self, input: impl BufRead, mut output: impl Write) -> Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.handle(&msg),
                Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } })),
            };
            if let Some(r) = reply {
                writeln!(output, "{r}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{GameInstall, Store};

    #[test]
    fn handshake_and_read_only_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("lib.sqlite3");
        let game_dir = tmp.path().join("game");
        std::fs::create_dir_all(game_dir.join("r6/logs")).unwrap();
        std::fs::write(game_dir.join("r6/logs/redscript_rCURRENT.log"), "line1\nline2\n[ERROR] boom\n").unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            let gi = GameInstall {
                path: game_dir.clone(),
                store: Store::Manual,
                proton_prefix: None,
                build_id: Some("7".into()),
                exe_file_version: None,
                exe_product_version: None,
                frameworks: vec![],
                launch_options: None,
                warnings: vec![],
            };
            let gid = db.upsert_game(&gi).unwrap();
            let g = db.game(gid).unwrap();
            let mid = db.insert_mod(&g, &crate::db::NewMod { name: "Alpha".into(), source: "manual".into(), ..Default::default() }).unwrap();
            db.set_touches(mid, analysis::SCANNER_VERSION, &[analysis::Touch {
                kind: Kind::RedsWrapMethod,
                key: "PlayerPuppet.OnGameAttached".into(),
                file: "r6/scripts/a.reds".into(),
            }])
            .unwrap();
        }
        let server = Server::new(Db::open_read_only(&db_path).unwrap());
        let input = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"find_touches","arguments":{"key":"playerpuppet"}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_log","arguments":{"game_id":1,"name":"r6/logs/redscript_rCURRENT.log","max_lines":1}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"read_log","arguments":{"game_id":1,"name":"../../../etc/passwd"}}}),
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"compatibility_report","arguments":{"game_id":1}}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let mut out = Vec::new();
        server.serve(input.as_bytes(), &mut out).unwrap();
        let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies.len(), 6, "notification gets no reply");
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], APP_NAME);
        assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 8);
        assert_eq!(replies[2]["result"]["structuredContent"]["results"][0]["mod"], "Alpha");
        assert_eq!(replies[3]["result"]["structuredContent"]["text"], "[ERROR] boom");
        assert_eq!(replies[4]["result"]["isError"], true);
        assert!(replies[5]["result"]["structuredContent"]["report"]["mods"].is_array());

        // The connection really can't write.
        assert!(server.db.set_setting("x", "y").is_err());
    }
}
