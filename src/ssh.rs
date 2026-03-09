use git2::{Cred, FetchOptions, RemoteCallbacks};
use std::fs;
use std::path::PathBuf;
use tracing::{debug, trace};

pub fn ssh_hostname(url: &str) -> Option<String> {
    if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.split_once('@').map_or(rest, |(_, r)| r);
        Some(rest.split(&['/', ':']).next()?.to_string())
    } else if url.contains('@') && url.contains(':') && !url.contains("://") {
        let after_at = url.split_once('@')?.1;
        Some(after_at.split(':').next()?.to_string())
    } else {
        None
    }
}

fn ssh_glob(pat: &[u8], hay: &[u8]) -> bool {
    match pat.first() {
        None => hay.is_empty(),
        Some(&b'*') => (0..=hay.len()).any(|i| ssh_glob(&pat[1..], &hay[i..])),
        Some(&b'?') => !hay.is_empty() && ssh_glob(&pat[1..], &hay[1..]),
        Some(&b) => !hay.is_empty() && b.eq_ignore_ascii_case(&hay[0]) && ssh_glob(&pat[1..], &hay[1..]),
    }
}

/// Parse `~/.ssh/config` and return IdentityFile paths for blocks whose Host
/// pattern matches `hostname`. Respects SSH glob wildcards (`*`, `?`).
fn ssh_config_identity_files(hostname: &str) -> Vec<PathBuf> {
    let config_path = dirs::home_dir().unwrap_or_default().join(".ssh").join("config");
    let Ok(content) = fs::read_to_string(&config_path) else { return Vec::new() };
    let home = dirs::home_dir().unwrap_or_default();

    let mut results = Vec::new();
    let mut in_match = false;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once(|c: char| c.is_whitespace()) else { continue };
        let val = val.trim();

        match key.to_ascii_lowercase().as_str() {
            "host" => {
                in_match = val.split_whitespace()
                    .any(|p| ssh_glob(p.as_bytes(), hostname.as_bytes()));
            }
            "identityfile" if in_match => {
                let path = val.replacen('~', &home.to_string_lossy(), 1);
                results.push(PathBuf::from(path));
            }
            _ => {}
        }
    }
    results
}

/// Fallback: scan top-level `~/.ssh/` for files matching the conventional
/// `id_*` naming scheme (no subdirectories, no `.pub` files).
fn discover_ssh_keys() -> Vec<PathBuf> {
    let ssh_dir = dirs::home_dir().unwrap_or_default().join(".ssh");
    let Ok(entries) = fs::read_dir(&ssh_dir) else { return Vec::new() };
    let mut keys: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().map(|e| e != "pub").unwrap_or(true)
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("id_"))
                    .unwrap_or(false)
        })
        .collect();
    keys.sort();
    keys
}

/// Credentials callback that cycles through all available options across retries.
///
/// For HTTPS URLs: tries anonymous credentials once; on a second invocation
/// (meaning the server rejected them) returns an error immediately to avoid
/// an infinite retry loop.
///
/// For SSH URLs: SSH agent (once) → IdentityFile entries from ~/.ssh/config
/// for the target host → id_* key files in ~/.ssh/. Keys that fail to load
/// are skipped. Returns an error once all candidates are exhausted.
pub fn make_auth_callbacks(remote_url: &str, explicit_key: Option<PathBuf>) -> RemoteCallbacks<'static> {
    use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};

    let is_https = remote_url.starts_with("http://") || remote_url.starts_with("https://");

    let key_files: Arc<Vec<PathBuf>> = Arc::new(if let Some(ref k) = explicit_key {
        vec![k.clone()]
    } else if let Some(host) = ssh_hostname(remote_url) {
        let from_config = ssh_config_identity_files(&host);
        if !from_config.is_empty() {
            debug!("SSH config: {} identity file(s) for {host}", from_config.len());
            from_config
        } else {
            discover_ssh_keys()
        }
    } else {
        discover_ssh_keys()
    });
    let agent_tried = Arc::new(AtomicBool::new(false));
    let key_idx = Arc::new(AtomicUsize::new(0));
    let https_tried = Arc::new(AtomicBool::new(false));

    let mut cb = RemoteCallbacks::new();
    cb.credentials(move |_url, username, allowed| {
        let user = username.unwrap_or("git");

        if allowed.contains(git2::CredentialType::SSH_KEY) {
            // Try SSH agent first (once), unless an explicit key was provided.
            if explicit_key.is_none() && !agent_tried.swap(true, Ordering::Relaxed) {
                if let Ok(c) = Cred::ssh_key_from_agent(user) {
                    trace!("SSH auth: agent");
                    return Ok(c);
                }
            }

            // Walk key files, skipping any that can't be loaded (e.g. wrong format).
            loop {
                let idx = key_idx.fetch_add(1, Ordering::Relaxed);
                let Some(path) = key_files.get(idx) else { break };
                debug!("SSH auth: trying {}", path.display());
                if let Ok(c) = Cred::ssh_key(user, None, path, None) {
                    return Ok(c);
                }
            }
        }

        if allowed.contains(git2::CredentialType::DEFAULT) {
            return Cred::default();
        }

        if allowed.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
            if !https_tried.swap(true, Ordering::Relaxed) {
                trace!("HTTPS auth: attempting anonymous");
                return Cred::userpass_plaintext(user, "");
            }
            return Err(git2::Error::from_str(
                "HTTPS authentication failed — use a credential helper or set GIT_ASKPASS",
            ));
        }

        let hint = if is_https {
            "HTTPS authentication failed — use a credential helper or set GIT_ASKPASS"
        } else if explicit_key.is_some() {
            "check the key path and permissions"
        } else {
            "use --key <path> to specify an SSH key, or load it into ssh-agent"
        };
        Err(git2::Error::from_str(&format!("authentication exhausted — {hint}")))
    });
    cb
}

pub fn make_fetch_options(remote_url: &str, explicit_key: Option<PathBuf>) -> FetchOptions<'static> {
    let mut fetch_opts = FetchOptions::new();
    fetch_opts.remote_callbacks(make_auth_callbacks(remote_url, explicit_key));
    fetch_opts
}
