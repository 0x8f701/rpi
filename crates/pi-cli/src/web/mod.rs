//! Web client assets embedded into the binary by build.rs.
//!
//! The table maps request paths to `(MIME type, bytes)` — currently one
//! self-contained `index.html` (vite-plugin-singlefile output) plus any
//! future split assets. See `build.rs` for the generation step and
//! `crates/pi-cli/web/` for the frontend project.

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

/// Look up an asset by request path. `/web` and `/` resolve to `index.html`;
/// named assets use their relative path (e.g. `/assets/index-hash.js`).
pub fn get(path: &str) -> Option<(&'static str, &'static [u8])> {
    let key = path.trim_start_matches('/');
    let key = if key.is_empty() || key == "web" { "index.html" } else { key };
    FILES
        .iter()
        .find(|(name, _, _)| *name == key)
        .map(|(_, mime, bytes)| (*mime, *bytes))
}

/// The main page (`index.html`) served at `GET /web`.
pub fn index() -> Option<(&'static str, &'static [u8])> {
    get("index.html")
}
