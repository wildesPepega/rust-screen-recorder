//! Game detection and naming.
//!
//! A window belongs to a game when its process was started by a game launcher
//! (Steam, Lutris, Heroic, Faugus, Bottles, umu …) – detected through the environment that the
//! launchers inject – and it is not itself a launcher/helper window (EA app, Rockstar launcher,
//! Ubisoft Connect, …). The name is resolved from the launcher's own library data.

use crate::config::GameSettings;
use crate::kwin::WindowInfo;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_LAUNCHER_BLOCKLIST: &[&str] = &[
    "*launcher",
    "*bootstrap",
    "*updater",
    "*patcher",
    "*crashhandler",
    "*crashreport",
    "*setup",
    "*installer",
    "*redist",
    "steam",
    "steam.exe",
    "steamwebhelper",
    "steamwebhelper.exe",
    "steamerrorreporter.exe",
    "eadesktop.exe",
    "eabackgroundservice.exe",
    "origin.exe",
    "epicgameslauncher.exe",
    "upc.exe",
    "ubisoftconnect.exe",
    "uplaywebcore.exe",
    "battle.net.exe",
    "agent.exe",
    "galaxyclient.exe",
    "riotclientservices.exe",
    "riotclientux.exe",
    "rockstarservice.exe",
    "socialclubhelper.exe",
    "playgtav.exe",
    "redprelauncher.exe",
    "dowser.exe",
    "paradox launcher.exe",
    "bethesda.net_launcher.exe",
    "explorer.exe",
    "conhost.exe",
    "winedevice.exe",
    "services.exe",
    "plugplay.exe",
    "rundll32.exe",
    "msiexec.exe",
    "wineconsole",
    "winecfg.exe",
    "easyanticheat_eos_setup.exe",
    "beservice.exe",
    "unitycrashhandler64.exe",
    "crashreportclient.exe",
    "xivlauncher",
    "lutris",
    "heroic",
    "faugus-launcher",
    "bottles",
    "zenity",
    "yad",
];

#[derive(Debug, Clone, PartialEq)]
pub struct GameInfo {
    pub name: String,
    pub source: String,
    pub exe: String,
}

struct ProcInfo {
    env: HashMap<String, String>,
    cmdline: Vec<String>,
    comm: String,
    ppid: i32,
}

fn read_proc(pid: i32) -> Option<ProcInfo> {
    let base = PathBuf::from(format!("/proc/{pid}"));
    let env = std::fs::read(base.join("environ"))
        .ok()
        .map(|b| {
            b.split(|c| *c == 0)
                .filter_map(|kv| {
                    let s = String::from_utf8_lossy(kv);
                    let (k, v) = s.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let cmdline = std::fs::read(base.join("cmdline"))
        .ok()?
        .split(|c| *c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    let comm = std::fs::read_to_string(base.join("comm")).unwrap_or_default().trim().to_string();
    let ppid = std::fs::read_to_string(base.join("stat"))
        .ok()
        .and_then(|s| {
            let rest = &s[s.rfind(')')? + 2..];
            rest.split_whitespace().nth(1)?.parse().ok()
        })
        .unwrap_or(0);
    Some(ProcInfo { env, cmdline, comm, ppid })
}

/// Basename of the real program (handles Wine/Proton "Z:\\path\\Game.exe").
fn exe_name(p: &ProcInfo) -> String {
    let first = p.cmdline.first().cloned().unwrap_or_default();
    let base = first.rsplit(['/', '\\']).next().unwrap_or("").to_string();
    let lower = base.to_lowercase();
    // wine64-preloader / wine: the program is the next argument
    if lower.starts_with("wine") || lower.contains("preloader") {
        if let Some(arg) = p.cmdline.get(1) {
            return arg.rsplit(['/', '\\']).next().unwrap_or(arg).to_string();
        }
    }
    if base.is_empty() {
        p.comm.clone()
    } else {
        base
    }
}

fn matches_list(list: &[String], exe: &str, caption: &str) -> bool {
    let exe_l = exe.to_lowercase();
    let exe_stem = exe_l.trim_end_matches(".exe");
    let cap_l = caption.to_lowercase();
    list.iter().any(|e| {
        let e = e.trim().to_lowercase();
        if e.is_empty() {
            return false;
        }
        if let Some(sub) = e.strip_prefix('*') {
            exe_stem.ends_with(sub) || exe_stem.contains(sub) || cap_l.ends_with(sub)
        } else {
            exe_l == e || exe_stem == e.trim_end_matches(".exe")
        }
    })
}

/// Walk up the process tree and collect the command lines (to find which launcher started us).
fn ancestry(mut pid: i32) -> Vec<String> {
    let mut v = vec![];
    for _ in 0..24 {
        if pid <= 1 {
            break;
        }
        let Some(p) = read_proc(pid) else { break };
        v.push(p.cmdline.join(" ").to_lowercase());
        pid = p.ppid;
    }
    v
}

pub fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| !matches!(c, '™' | '®' | '©'))
        // Keep the name exactly like the game's title – only characters that are invalid in
        // Linux file names are replaced.
        .map(|c| if c.is_control() || c == '/' { '_' } else { c })
        .collect();
    let s = s.trim().trim_matches('.').trim().to_string();
    if s.is_empty() {
        "Unknown Game".into()
    } else {
        s.chars().take(120).collect()
    }
}

/// Decide whether `w` is a game window and find the game's name.
pub fn detect(w: &WindowInfo, cfg: &GameSettings) -> Option<GameInfo> {
    if w.pid <= 0 || !w.normal {
        return None;
    }
    if (w.w as u32) < cfg.min_width || (w.h as u32) < cfg.min_height {
        return None;
    }
    let p = read_proc(w.pid)?;
    let exe = exe_name(&p);
    if exe.to_lowercase().contains("rust-screen-recorder") {
        return None;
    }
    if matches_list(&cfg.launcher_blocklist, &exe, &w.caption) {
        return None;
    }
    let env = &p.env;
    let fallback = || {
        if !w.caption.trim().is_empty() {
            w.caption.clone()
        } else {
            exe.trim_end_matches(".exe").to_string()
        }
    };
    let mk = |name: String, source: &str| Some(GameInfo { name: sanitize(&name), source: source.into(), exe: exe.clone() });

    // --- Steam (also covers non-Steam shortcuts added to Steam, e.g. by Faugus/Lutris) --------
    let app_id = env.get("SteamAppId").or_else(|| env.get("STEAM_COMPAT_APP_ID")).cloned().unwrap_or_default();
    let game_id = env.get("SteamGameId").cloned().unwrap_or_default();
    if (!app_id.is_empty() && app_id != "0") || (!game_id.is_empty() && game_id != "0") {
        let gid: u64 = game_id.parse().unwrap_or(0);
        if gid > u32::MAX as u64 {
            let short = (gid >> 32) as u32;
            if let Some(n) = steam_shortcut_name(short) {
                return mk(n, "Steam (shortcut)");
            }
        }
        let id = if !app_id.is_empty() && app_id != "0" { app_id.clone() } else { game_id.clone() };
        if let Some(n) = steam_app_name(&id) {
            return mk(n, "Steam");
        }
        // Lutris/Heroic games sometimes also carry a Steam id; fall through to them first.
        if !env.contains_key("LUTRIS_GAME_UUID") && !env.contains_key("HEROIC_APP_NAME") {
            return mk(fallback(), "Steam");
        }
    }

    // --- Lutris ------------------------------------------------------------------------------
    if env.contains_key("LUTRIS_GAME_UUID") || env.contains_key("GAME_NAME") {
        if let Some(n) = env.get("GAME_NAME").filter(|s| !s.is_empty()) {
            return mk(n.clone(), "Lutris");
        }
        return mk(fallback(), "Lutris");
    }

    // --- Heroic --------------------------------------------------------------------------------
    let anc = ancestry(w.pid);
    let heroic_id = env.get("HEROIC_APP_NAME").cloned();
    if heroic_id.is_some() || anc.iter().any(|c| c.contains("heroic")) {
        if let Some(id) = heroic_id.as_deref().or(env.get("LEGENDARY_APP_NAME").map(|s| s.as_str())) {
            if let Some(n) = heroic_title(id) {
                return mk(n, "Heroic");
            }
        }
        return mk(fallback(), "Heroic");
    }

    // --- Faugus Launcher (matched by the game's own Wine prefix / exe path) --------------------
    let faugus = faugus_title(env.get("WINEPREFIX").map(|s| s.as_str()), &p.cmdline);
    if let Some(n) = faugus {
        return mk(n, "Faugus");
    }
    if env.keys().any(|k| k.starts_with("FAUGUS")) || anc.iter().any(|c| c.contains("faugus")) {
        return mk(fallback(), "Faugus");
    }

    // --- Bottles / umu / generic Proton -------------------------------------------------------
    if env.contains_key("BOTTLE_NAME") || anc.iter().any(|c| c.contains("bottles")) {
        return mk(fallback(), "Bottles");
    }
    if env.contains_key("UMU_ID") || env.contains_key("GAMEID") || env.contains_key("STEAM_COMPAT_DATA_PATH") {
        return mk(fallback(), "Proton");
    }

    // --- User list / fullscreen fallback --------------------------------------------------------
    if matches_list(&cfg.extra_games, &exe, "") {
        return mk(fallback(), "Custom");
    }
    if cfg.fullscreen_is_game && w.fullscreen {
        return mk(fallback(), "Fullscreen");
    }
    None
}

// ------------------------------------------------------------------------------------------------
// Steam
// ------------------------------------------------------------------------------------------------

fn steam_roots() -> Vec<PathBuf> {
    let Some(h) = dirs::home_dir() else { return vec![] };
    let mut v: Vec<PathBuf> = [
        ".local/share/Steam",
        ".steam/steam",
        ".steam/root",
        ".var/app/com.valvesoftware.Steam/.local/share/Steam",
        "snap/steam/common/.local/share/Steam",
    ]
    .iter()
    .map(|p| h.join(p))
    .filter(|p| p.exists())
    .filter_map(|p| p.canonicalize().ok())
    .collect();
    v.dedup();
    v
}

/// Parse `"key" "value"` pairs from a text VDF/ACF file (flat, good enough for our keys).
fn vdf_values(text: &str, key: &str) -> Vec<String> {
    let mut out = vec![];
    for line in text.lines() {
        let parts: Vec<&str> = line.split('"').collect();
        // ["\t", key, "\t\t", value, ""]
        if parts.len() >= 5 && parts[1].eq_ignore_ascii_case(key) {
            out.push(parts[3].replace("\\\\", "\\"));
        }
    }
    out
}

fn steam_libraries() -> Vec<PathBuf> {
    let mut libs = vec![];
    for root in steam_roots() {
        libs.push(root.clone());
        if let Ok(t) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            libs.extend(vdf_values(&t, "path").into_iter().map(PathBuf::from));
        }
    }
    libs.sort();
    libs.dedup();
    libs
}

pub fn steam_app_name(app_id: &str) -> Option<String> {
    for lib in steam_libraries() {
        let f = lib.join(format!("steamapps/appmanifest_{app_id}.acf"));
        if let Ok(t) = std::fs::read_to_string(&f) {
            if let Some(n) = vdf_values(&t, "name").into_iter().next() {
                return Some(n);
            }
        }
    }
    None
}

/// Look up a non-Steam shortcut in userdata/*/config/shortcuts.vdf (binary VDF).
fn steam_shortcut_name(app_id: u32) -> Option<String> {
    for root in steam_roots() {
        let Ok(rd) = std::fs::read_dir(root.join("userdata")) else { continue };
        for user in rd.flatten() {
            let f = user.path().join("config/shortcuts.vdf");
            if let Ok(b) = std::fs::read(&f) {
                for (id, name) in parse_shortcuts(&b) {
                    if id == app_id {
                        return Some(name);
                    }
                }
            }
        }
    }
    None
}

fn parse_shortcuts(b: &[u8]) -> Vec<(u32, String)> {
    // Minimal binary VDF walker: 0x00 = map, 0x01 = string, 0x02 = int32, 0x08 = end.
    fn cstr(b: &[u8], i: &mut usize) -> Option<String> {
        let start = *i;
        while *i < b.len() && b[*i] != 0 {
            *i += 1;
        }
        let s = String::from_utf8_lossy(b.get(start..*i)?).to_string();
        *i += 1;
        Some(s)
    }
    let mut out = vec![];
    let mut i = 0usize;
    let mut cur_id: Option<u32> = None;
    let mut cur_name: Option<String> = None;
    while i < b.len() {
        let t = b[i];
        i += 1;
        match t {
            0x00 => {
                if cstr(b, &mut i).is_none() {
                    break;
                }
            }
            0x01 => {
                let (Some(k), Some(v)) = (cstr(b, &mut i), cstr(b, &mut i)) else { break };
                if k.eq_ignore_ascii_case("appname") {
                    cur_name = Some(v);
                }
            }
            0x02 => {
                let Some(k) = cstr(b, &mut i) else { break };
                if i + 4 > b.len() {
                    break;
                }
                let v = u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
                i += 4;
                if k.eq_ignore_ascii_case("appid") {
                    cur_id = Some(v);
                }
            }
            0x08 => {
                if let (Some(id), Some(n)) = (cur_id.take(), cur_name.take()) {
                    out.push((id, n));
                }
            }
            _ => break,
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------
// Heroic
// ------------------------------------------------------------------------------------------------

fn heroic_title(app_name: &str) -> Option<String> {
    let h = dirs::home_dir()?;
    let roots = [h.join(".config/heroic"), h.join(".var/app/com.heroicgameslauncher.hgl/config/heroic")];
    let files = [
        "store_cache/legendary_library.json",
        "store_cache/gog_library.json",
        "store_cache/nile_library.json",
        "sideload_apps/library.json",
        "legendaryConfig/legendary/metadata",
    ];
    for r in &roots {
        for f in files {
            let p = r.join(f);
            if p.is_file() {
                if let Some(t) = json_find_title(&p, app_name) {
                    return Some(t);
                }
            }
        }
        // Legendary metadata: one json per game, named <app_name>.json, with app_title
        let meta = r.join(format!("legendaryConfig/legendary/metadata/{app_name}.json"));
        if let Ok(s) = std::fs::read_to_string(meta) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                if let Some(t) = v.get("app_title").and_then(|t| t.as_str()) {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn json_find_title(p: &Path, app_name: &str) -> Option<String> {
    let s = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    fn walk(v: &serde_json::Value, id: &str) -> Option<String> {
        match v {
            serde_json::Value::Object(m) => {
                let matches = ["app_name", "appName", "id"]
                    .iter()
                    .any(|k| m.get(*k).and_then(|x| x.as_str()) == Some(id));
                if matches {
                    if let Some(t) = m.get("title").and_then(|t| t.as_str()) {
                        return Some(t.to_string());
                    }
                }
                m.values().find_map(|x| walk(x, id))
            }
            serde_json::Value::Array(a) => a.iter().find_map(|x| walk(x, id)),
            _ => None,
        }
    }
    walk(&v, app_name)
}

// ------------------------------------------------------------------------------------------------
// Faugus Launcher
// ------------------------------------------------------------------------------------------------

fn faugus_title(wineprefix: Option<&str>, cmdline: &[String]) -> Option<String> {
    let h = dirs::home_dir()?;
    let candidates = [
        h.join(".local/share/faugus-launcher/games.json"),
        h.join(".config/faugus-launcher/games.json"),
        h.join(".var/app/io.github.Faugus.faugus-launcher/data/faugus-launcher/games.json"),
        h.join(".var/app/io.github.Faugus.faugus-launcher/config/faugus-launcher/games.json"),
    ];
    let norm = |p: &str| p.trim_end_matches('/').to_string();
    let prefix = wineprefix.map(norm);
    let joined = cmdline.join(" ").replace('\\', "/").to_lowercase();
    for c in candidates {
        let Ok(s) = std::fs::read_to_string(&c) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else { continue };
        let list: Vec<&serde_json::Value> = match &v {
            serde_json::Value::Array(a) => a.iter().collect(),
            serde_json::Value::Object(m) => m.values().collect(),
            _ => vec![],
        };
        // 1) exact exe match, 2) unique Wine prefix match
        let mut by_prefix = vec![];
        for g in &list {
            let Some(title) = g.get("title").and_then(|t| t.as_str()) else { continue };
            if let Some(path) = g.get("path").and_then(|x| x.as_str()) {
                let base = path.rsplit(['/', '\\']).next().unwrap_or("").to_lowercase();
                if !base.is_empty() && joined.contains(&base) {
                    return Some(title.to_string());
                }
            }
            if let (Some(pfx), Some(gp)) = (&prefix, g.get("prefix").and_then(|x| x.as_str())) {
                if *pfx == norm(gp) {
                    by_prefix.push(title.to_string());
                }
            }
        }
        if by_prefix.len() == 1 {
            return by_prefix.pop();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocklist() {
        let l: Vec<String> = DEFAULT_LAUNCHER_BLOCKLIST.iter().map(|s| s.to_string()).collect();
        assert!(matches_list(&l, "Launcher.exe", ""));
        assert!(matches_list(&l, "RockstarLauncher.exe", ""));
        assert!(matches_list(&l, "EADesktop.exe", ""));
        assert!(!matches_list(&l, "GTA5.exe", "Grand Theft Auto V"));
        assert!(!matches_list(&l, "eldenring.exe", "ELDEN RING"));
    }

    #[test]
    fn names() {
        assert_eq!(sanitize("Half-Life 2: Episode One™"), "Half-Life 2: Episode One");
        assert_eq!(sanitize("AC/DC Live"), "AC_DC Live");
        assert_eq!(sanitize("  "), "Unknown Game");
    }

    #[test]
    fn vdf() {
        let t = "\"AppState\"\n{\n\t\"appid\"\t\t\"570\"\n\t\"name\"\t\t\"Dota 2\"\n}";
        assert_eq!(vdf_values(t, "name"), vec!["Dota 2"]);
    }

    #[test]
    fn shortcuts() {
        let mut b = vec![0u8];
        b.extend(b"shortcuts\0");
        b.push(0);
        b.extend(b"0\0");
        b.push(2);
        b.extend(b"appid\0");
        b.extend(123456u32.to_le_bytes());
        b.push(1);
        b.extend(b"AppName\0My Game\0");
        b.push(8);
        b.push(8);
        assert_eq!(parse_shortcuts(&b), vec![(123456, "My Game".to_string())]);
    }

    #[test]
    fn wine_exe() {
        let p = ProcInfo {
            env: Default::default(),
            cmdline: vec!["Z:\\games\\Elden Ring\\Game\\eldenring.exe".into()],
            comm: "eldenring.exe".into(),
            ppid: 1,
        };
        assert_eq!(exe_name(&p), "eldenring.exe");
    }
}
