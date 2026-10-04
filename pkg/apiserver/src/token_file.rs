//! Static bearer tokens from `--token-auth-file` (#188).
//!
//! The file is kube-apiserver's `--token-auth-file` format, one token per
//! line, CSV:
//!
//! ```text
//! token,user,uid[,"group1,group2,..."]
//! ```
//!
//! stormpump writes it on first boot from install-config's `apiToken`
//! (stormpump#78: `/state/config/token-auth.csv`, mode 0600, the line
//! `<token>,system:admin,system:admin,"system:masters"`), and `sc` and the
//! console log in with that token.
//!
//! Unlike upstream, which reads the file once and refuses to start on a bad
//! one, the file is **followed**: it may appear after the apiserver starts,
//! be rewritten, or be removed. The authenticator always holds the last
//! well-formed contents — none at all until one has been read, and none again
//! once the file is gone, so removing it revokes its tokens. A file that is
//! present but malformed is reported and the previous set is kept, as a bad
//! serving certificate is (`tls::watch_cert_files`). A token is never logged.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How often the file is re-read. Short, because removing a line is how a
/// token is revoked.
const RELOAD_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct Entry {
    token: Vec<u8>,
    username: String,
    groups: Vec<String>,
}

/// The tokens currently accepted. Cheap to clone; clones share the set.
#[derive(Clone, Default)]
pub struct StaticTokens(Arc<RwLock<Vec<Entry>>>);

impl StaticTokens {
    /// No tokens: every lookup fails. What the apiserver has without the flag.
    pub fn none() -> Self {
        Self::default()
    }

    /// The username and groups `token` authenticates as, if it is in the file.
    ///
    /// Every entry is compared, each in constant time, so neither which entry
    /// matched nor how much of a token was right shows in the timing.
    pub fn authenticate(&self, token: &str) -> Option<(String, Vec<String>)> {
        if token.is_empty() {
            return None;
        }
        let entries = self.0.read().unwrap_or_else(|e| e.into_inner());
        let mut found = None;
        for e in entries.iter() {
            if constant_time_eq(&e.token, token.as_bytes()) && found.is_none() {
                found = Some((e.username.clone(), e.groups.clone()));
            }
        }
        found
    }

    /// How many tokens are accepted.
    pub fn len(&self) -> usize {
        self.0.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A set parsed from `text`, for tests elsewhere in the crate.
    #[cfg(test)]
    pub(crate) fn from_text(text: &str) -> Self {
        let t = Self::none();
        t.replace(parse(text.as_bytes()).unwrap());
        t
    }

    fn replace(&self, entries: Vec<Entry>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = entries;
    }

    /// Re-read `path` and apply what it says. Returns whether the set changed
    /// or was confirmed, for logging; `last` holds the bytes last applied
    /// (`None` = no file).
    fn refresh(&self, path: &Path, last: &mut Option<Vec<u8>>) {
        match std::fs::read(path) {
            Ok(bytes) => {
                if last.as_deref() == Some(bytes.as_slice()) {
                    return;
                }
                match parse(&bytes) {
                    Ok(entries) => {
                        let n = entries.len();
                        self.replace(entries);
                        *last = Some(bytes);
                        tracing::info!(path = %path.display(), tokens = n, "token auth file loaded");
                    }
                    Err(e) => {
                        // Remember the bad bytes so the warning is not repeated
                        // every interval; a later good write still applies.
                        *last = Some(bytes);
                        tracing::warn!(
                            path = %path.display(),
                            "token auth file is malformed, keeping the {} token(s) already loaded: {e}",
                            self.len(),
                        );
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if last.is_some() || !self.is_empty() {
                    self.replace(Vec::new());
                    tracing::info!(path = %path.display(), "token auth file removed; its tokens are revoked");
                }
                *last = None;
            }
            Err(e) => tracing::warn!(
                path = %path.display(),
                "token auth file unreadable, keeping the {} token(s) already loaded: {e}",
                self.len(),
            ),
        }
    }

    /// Load `path` now and follow it for the life of the process.
    pub fn follow(path: PathBuf) -> Self {
        let tokens = Self::none();
        let mut last = None;
        tokens.refresh(&path, &mut last);
        if last.is_none() {
            tracing::warn!(
                path = %path.display(),
                "token auth file does not exist yet; no static tokens until it does",
            );
        }
        let t = tokens.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RELOAD_INTERVAL).await;
                t.refresh(&path, &mut last);
            }
        });
        tokens
    }
}

/// Byte equality whose time depends only on the lengths, as Go's
/// `subtle.ConstantTimeCompare`, which upstream uses for this file.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Parse the whole file. Any bad line rejects the file, as upstream does.
fn parse(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8".to_string())?;
    let mut entries: Vec<Entry> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let fields = split_csv(line).map_err(|e| format!("line {n}: {e}"))?;
        if fields.len() < 3 {
            return Err(format!("line {n}: want token,user,uid[,groups], got {} field(s)", fields.len()));
        }
        let token = fields[0].trim();
        let username = fields[1].trim();
        if token.is_empty() {
            return Err(format!("line {n}: empty token"));
        }
        if username.is_empty() {
            return Err(format!("line {n}: empty user"));
        }
        let groups = fields
            .get(3)
            .map(|g| {
                g.split(',')
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        // A repeated token: the later line wins, as upstream (which warns).
        if let Some(pos) = entries.iter().position(|e| e.token == token.as_bytes()) {
            tracing::warn!("token auth file: line {n} repeats an earlier token; the later line wins");
            entries.remove(pos);
        }
        entries.push(Entry { token: token.as_bytes().to_vec(), username: username.into(), groups });
    }
    Ok(entries)
}

/// Split one CSV record (RFC 4180 quoting: `"a,b"`, `""` for a quote).
/// Leading spaces before a field are ignored, as Go's `TrimLeadingSpace`.
fn split_csv(line: &str) -> Result<Vec<String>, String> {
    let mut fields = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek() == Some(&' ') || chars.peek() == Some(&'\t') {
            chars.next();
        }
        let mut field = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        field.push('"');
                    }
                    Some('"') => break,
                    Some(c) => field.push(c),
                    None => return Err("unterminated quote".into()),
                }
            }
            match chars.next() {
                None => {
                    fields.push(field);
                    return Ok(fields);
                }
                Some(',') => fields.push(field),
                Some(_) => return Err("text after a closing quote".into()),
            }
        } else {
            loop {
                match chars.next() {
                    None => {
                        fields.push(field);
                        return Ok(fields);
                    }
                    Some(',') => break,
                    Some('"') => return Err("bare quote in an unquoted field".into()),
                    Some(c) => field.push(c),
                }
            }
            fields.push(field);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line stormpump#78 writes.
    const STORMPUMP: &str = "0123456789abcdef0123456789abcdef0123456789abcdef,system:admin,system:admin,\"system:masters\"\n";

    fn loaded(text: &str) -> StaticTokens {
        StaticTokens::from_text(text)
    }

    #[test]
    fn the_stormpump_line_is_system_admin_in_system_masters() {
        let t = loaded(STORMPUMP);
        let (user, groups) = t
            .authenticate("0123456789abcdef0123456789abcdef0123456789abcdef")
            .expect("token refused");
        assert_eq!(user, "system:admin");
        assert_eq!(groups, ["system:masters"]);
    }

    #[test]
    fn another_token_is_refused() {
        let t = loaded(STORMPUMP);
        assert!(t.authenticate("0123456789abcdef0123456789abcdef0123456789abcdee").is_none());
        assert!(t.authenticate("0123456789abcdef").is_none());
        assert!(t.authenticate("").is_none());
        assert!(StaticTokens::none().authenticate("anything").is_none());
    }

    #[test]
    fn quoted_groups_split_on_commas_and_groups_are_optional() {
        let t = loaded("tok1,alice,1001,\"dev, ops\"\n# a comment\n\ntok2,bob,1002\r\n");
        assert_eq!(t.authenticate("tok1").unwrap(), ("alice".into(), vec!["dev".into(), "ops".into()]));
        assert_eq!(t.authenticate("tok2").unwrap(), ("bob".into(), vec![]));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn a_repeated_token_takes_the_later_line() {
        let t = loaded("tok,alice,1\ntok,bob,2\n");
        assert_eq!(t.len(), 1);
        assert_eq!(t.authenticate("tok").unwrap().0, "bob");
    }

    #[test]
    fn a_bad_line_rejects_the_file() {
        for bad in [
            "tok,alice\n",              // too few fields
            ",alice,1\n",               // empty token
            "tok,,1\n",                 // empty user
            "tok,alice,1,\"masters\n",  // unterminated quote
            "tok,alice,1,\"a\"b\n",     // text after the quote
            "to\"k,alice,1\n",          // bare quote
        ] {
            assert!(parse(bad.as_bytes()).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn constant_time_eq_is_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn the_file_is_followed_written_late_bad_rewritten_and_removed() {
        let dir = std::env::temp_dir().join(format!("rk-token-file-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token-auth.csv");
        let t = StaticTokens::none();
        let mut last = None;

        // Not there yet: nothing accepted.
        t.refresh(&path, &mut last);
        assert!(t.is_empty());

        // First boot writes it.
        std::fs::write(&path, "tokA,system:admin,system:admin,\"system:masters\"\n").unwrap();
        t.refresh(&path, &mut last);
        assert_eq!(t.authenticate("tokA").unwrap().0, "system:admin");

        // A malformed rewrite keeps what works.
        std::fs::write(&path, "tokB,system:admin\n").unwrap();
        t.refresh(&path, &mut last);
        assert!(t.authenticate("tokA").is_some());
        assert!(t.authenticate("tokB").is_none());

        // A good rewrite replaces the token: the old one stops working.
        std::fs::write(&path, "tokC,system:admin,system:admin,\"system:masters\"\n").unwrap();
        t.refresh(&path, &mut last);
        assert!(t.authenticate("tokA").is_none());
        assert!(t.authenticate("tokC").is_some());

        // An install-config without apiToken removes the file: revoked.
        std::fs::remove_file(&path).unwrap();
        t.refresh(&path, &mut last);
        assert!(t.authenticate("tokC").is_none());
        assert!(t.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
