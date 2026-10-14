//! Secret and path redaction for diagnostics and generated reports.
//!
//! Redaction is best-effort by design and documented as such: it removes common
//! credential shapes from text that would otherwise be copied into shared
//! reports. It does not attempt to be a general data-loss-prevention system.

/// Replace absolute machine paths in free text with portable placeholders.
///
/// - the repository root becomes `<repo>`
/// - the user's home directory becomes `~`
///
/// On Windows, `Path::canonicalize` returns an extended-length (`\\?\`) path
/// that will not appear in ordinary log text, so both the canonical and the
/// literal (as-given) forms are replaced.
pub fn redact_paths(
    text: &str,
    repo_root: &std::path::Path,
    home: Option<&std::path::Path>,
) -> String {
    let mut out = text.to_string();
    for root in path_variants(repo_root) {
        out = replace_all_ci(&out, &root, "<repo>");
        out = replace_all_ci(&out, &root.replace('\\', "/"), "<repo>");
    }
    if let Some(h) = home {
        for hv in path_variants(h) {
            out = replace_all_ci(&out, &hv, "~");
            out = replace_all_ci(&out, &hv.replace('\\', "/"), "~");
        }
    }
    out
}

/// All plausible on-disk spellings of a path: the literal form, plus the
/// canonicalized form with and without the Windows `\\?\` prefix.
fn path_variants(p: &std::path::Path) -> Vec<String> {
    let mut v = Vec::new();
    let raw = p.display().to_string();
    if !raw.is_empty() {
        v.push(raw);
    }
    if let Ok(c) = p.canonicalize() {
        let cs = c.display().to_string();
        if let Some(stripped) = cs.strip_prefix(r"\\?\") {
            let s = stripped.to_string();
            if !s.is_empty() && !v.contains(&s) {
                v.push(s);
            }
        }
        if !cs.is_empty() && !v.contains(&cs) {
            v.push(cs);
        }
    }
    v
}

/// Redact likely credentials from free text.
pub fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let lower = text.to_lowercase();

    // Sensitive `key: value` / `key = value` pairs.
    const KEYS: &[&str] = &[
        "password",
        "passwd",
        "secret",
        "token",
        "api_key",
        "apikey",
        "access_key",
        "auth",
        "authorization",
        "client_secret",
        "private_key",
    ];
    let bytes: Vec<char> = text.chars().collect();
    let low_chars: Vec<char> = lower.chars().collect();
    let mut i = 0usize;
    let mut redacted_ranges: Vec<(usize, usize)> = Vec::new();

    while i < low_chars.len() {
        // Word-boundary check before a key.
        let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        for key in KEYS {
            let k: Vec<char> = key.chars().collect();
            if i + k.len() > low_chars.len() {
                continue;
            }
            if (i == 0 || !is_word(low_chars[i - 1]))
                && low_chars[i..i + k.len()] == k[..]
                && (i + k.len() >= low_chars.len() || !is_word(low_chars[i + k.len()]))
            {
                let mut j = i + k.len();
                while j < low_chars.len() && low_chars[j].is_whitespace() {
                    j += 1;
                }
                if j < low_chars.len() && (low_chars[j] == ':' || low_chars[j] == '=') {
                    j += 1;
                    while j < low_chars.len()
                        && (low_chars[j].is_whitespace() || low_chars[j] == '"')
                    {
                        j += 1;
                    }
                    // Value: up to whitespace, quote, comma or end of line.
                    let start = j;
                    while j < bytes.len()
                        && !bytes[j].is_whitespace()
                        && !matches!(bytes[j], '"' | '\'' | ',' | ')' | ']')
                        && bytes[j] != '\n'
                    {
                        j += 1;
                    }
                    if j > start {
                        redacted_ranges.push((start, j));
                    }
                    i = j;
                    break;
                }
            }
        }
        i += 1;
    }

    redacted_ranges.sort();
    redacted_ranges.dedup();
    let mut prev_end = 0usize;
    for (start, end) in redacted_ranges {
        if start < prev_end {
            continue;
        }
        out.push_str(&chars_to_string(&bytes[prev_end..start]));
        out.push_str("[REDACTED]");
        prev_end = end;
    }
    out.push_str(&chars_to_string(&bytes[prev_end.min(bytes.len())..]));

    // Token-shaped strings (AWS, GitHub, Slack, JWT, private keys).
    mask_token_shapes(&out)
}

fn chars_to_string(chars: &[char]) -> String {
    chars.iter().collect()
}

/// Mask credential-shaped substrings, keeping a short prefix for identification.
fn mask_token_shapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if let Some(len) = token_len_at(&chars, i) {
            let tok: String = chars[i..i + len].iter().collect();
            let keep = tok.len().min(6);
            out.push_str(&tok[..keep]);
            out.push_str("[REDACTED]");
            i += len;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// If a known credential shape starts at `i`, return its length in chars.
fn token_len_at(chars: &[char], i: usize) -> Option<usize> {
    let tail: String = chars[i..].iter().collect::<String>();
    let lt = tail.to_lowercase();

    // PEM private key block: mask the whole block.
    if lt.starts_with("-----begin") {
        if let Some(end) = tail.to_lowercase().find("-----end") {
            let block_end = tail[end..]
                .find("-----")
                .map(|p| end + p + 5)
                .unwrap_or(tail.len());
            // Only treat as key if it mentions PRIVATE KEY.
            if lt[..block_end.min(lt.len())].contains("private key") {
                return Some(block_end);
            }
        }
    }
    const AWS: &str = "akia";
    if lt.starts_with(AWS)
        && tail.len() >= 20
        && tail[4..]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return Some(20);
    }
    for pfx in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"] {
        if lt.starts_with(pfx) {
            let rest_len = pfx.len();
            let body: String = tail[rest_len..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if body.len() >= 30 {
                return Some(rest_len + body.len());
            }
        }
    }
    for pfx in [
        "xoxb-", "xoxp-", "xoxa-", "xoxs-", "sk-", "sk_live_", "pk_live_",
    ] {
        if lt.starts_with(pfx) {
            let rest_len = pfx.len();
            let body: String = tail[rest_len..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if body.len() >= 20 {
                return Some(rest_len + body.len());
            }
        }
    }
    // JWT: three base64url segments starting with eyj.
    if tail.starts_with("eyJ") {
        let segs: Vec<&str> = tail.split('.').collect();
        if segs.len() >= 3 && segs[0].len() > 10 && segs[1].len() > 10 {
            // The signature segment ends at the first non-base64url char
            // (e.g. trailing whitespace or punctuation), so measure it
            // explicitly rather than trusting `segs[2]`'s full length.
            let sig_len = segs[2]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                .count();
            if sig_len >= 10 {
                let len = segs[0].len() + 1 + segs[1].len() + 1 + sig_len;
                return Some(len);
            }
        }
    }
    None
}

/// Case-insensitive literal replacement.
fn replace_all_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let hay_lower = haystack.to_lowercase();
    let needle_lower = needle.to_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut last = 0usize;
    let mut idx = 0usize;
    while let Some(pos) = hay_lower[idx..].find(&needle_lower) {
        let start = idx + pos;
        out.push_str(&haystack[last..start]);
        out.push_str(replacement);
        idx = start + needle.len();
        last = idx;
        if idx >= haystack.len() {
            break;
        }
    }
    out.push_str(&haystack[last.min(haystack.len())..]);
    out
}

/// Convenience: apply both redactions, in the safe order (paths first).
pub fn sanitize(text: &str, repo_root: &std::path::Path, home: Option<&std::path::Path>) -> String {
    redact_secrets(&redact_paths(text, repo_root, home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_key_value_secrets() {
        let s = redact_secrets("password: hunter22 and api_key=abc123secret");
        assert!(!s.contains("hunter22"), "got: {s}");
        assert!(!s.contains("abc123secret"), "got: {s}");
        assert!(s.contains("[REDACTED]"));
        assert!(s.contains("password:"), "keys stay for context");
    }

    #[test]
    fn redacts_github_and_aws_tokens() {
        let s = redact_secrets(
            "token ghp_1234567890abcdefghijklmnopqrstuv123456 and AKIAIOSFODNN7EXAMPLE",
        );
        assert!(
            !s.contains("ghp_1234567890abcdefghijklmnopqrstuv123456"),
            "got: {s}"
        );
        assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"), "got: {s}");
    }

    #[test]
    fn redacts_jwt_and_pem() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N65IhHHRv1F";
        let s = redact_secrets(&format!("auth {jwt} end"));
        assert!(!s.contains("eyJzdWIi"), "jwt payload must be masked: {s}");

        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----";
        let s = redact_secrets(pem);
        assert!(!s.contains("MIIEpAIBAAKCAQEA"), "got: {s}");
    }

    #[test]
    fn leaves_normal_text_alone() {
        let text = "the function parseConfig reads a password field from the file";
        let s = redact_secrets(text);
        assert!(s.contains("parseConfig"));
        assert!(
            s.contains("password field"),
            "no value follows a key here: {s}"
        );
    }

    #[test]
    fn path_redaction_replaces_repo_root() {
        let dir = std::env::temp_dir().join("aftermath-redact-test");
        std::fs::create_dir_all(&dir).unwrap();
        let text = format!("error at {}", dir.join("src").join("a.ts").display());
        let out = redact_paths(&text, &dir, None);
        assert!(
            !out.contains(&dir.display().to_string()),
            "root must be replaced: {out}"
        );
        assert!(out.contains("<repo>"), "got: {out}");
    }

    #[test]
    fn sanitize_applies_both() {
        let dir = std::env::temp_dir().join("aftermath-redact-test2");
        std::fs::create_dir_all(&dir).unwrap();
        let text = format!(
            "{}/key token=ghp_1234567890abcdefghij1234567890abcd",
            dir.display()
        );
        let out = sanitize(&text, &dir, None);
        assert!(!out.contains("ghp_"));
        assert!(!out.contains(&dir.display().to_string()));
    }
}
