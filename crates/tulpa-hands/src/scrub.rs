//! The Mac-side scrub.
//!
//! The secret shapes run over every hand reply before it leaves. Same two
//! tiers as the server's secret patterns: specific shapes anywhere; long
//! base64/hex runs on a line that names a key, token, secret or password (or
//! the line after one). A PEM private key goes whole. Keep the shape list
//! equal to the server's when either changes.

use std::sync::LazyLock;

use regex::Regex;

/// What replaces a secret.
const SECRET: &str = "[secret]";

const SPECIFIC_SHAPES: &[&str] = &[
    "-----BEGIN ([A-Z]+ )?PRIVATE KEY-----",
    "sk-ant-[a-z]+[0-9]*-[A-Za-z0-9_-]{20,}",
    "sk-(proj-)?[A-Za-z0-9_-]{32,}",
    "xox[abprs]-[A-Za-z0-9-]{10,}",
    "gh[pousr]_[A-Za-z0-9]{36}",
    "github_pat_[A-Za-z0-9_]{40,}",
    "[0-9]{8,10}:AA[A-Za-z0-9_-]{33}",
    "AGE-SECRET-KEY-1[0-9A-Z]{58}",
    "AKIA[0-9A-Z]{16}",
    "DO00[0-9A-Z]{16,}",
    "eyJ[A-Za-z0-9_-]{10,}[.]eyJ[A-Za-z0-9_-]{10,}[.][A-Za-z0-9_-]{10,}",
];

struct Shapes {
    specific: Regex,
    pem_block: Regex,
    context: Regex,
    long_run: Regex,
}

/// `None` only if a pattern this file owns fails to compile (the tests compile
/// them all); the scrub then fails closed and nothing leaves.
static SHAPES: LazyLock<Option<Shapes>> = LazyLock::new(|| {
    Some(Shapes {
        specific: Regex::new(&SPECIFIC_SHAPES.join("|")).ok()?,
        pem_block: Regex::new(
            r"(?s)-----BEGIN ([A-Z]+ )?PRIVATE KEY-----.*?(-----END ([A-Z]+ )?PRIVATE KEY-----|\z)",
        )
        .ok()?,
        context: Regex::new(
            r"(?i)key|token|secret|passw|pwd|credential|AKIA[0-9A-Z]{16}|DO00[0-9A-Z]{16,}",
        )
        .ok()?,
        long_run: Regex::new(r"[A-Za-z0-9/+=_-]{32,}").ok()?,
    })
});

/// The text with every credential-shaped string replaced by `[secret]`;
/// `None` when the scrub itself is unavailable (then nothing may leave).
#[must_use]
pub fn scrub_secrets(text: &str) -> Option<String> {
    let shapes = SHAPES.as_ref()?;
    let text = shapes.pem_block.replace_all(text, SECRET);
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut previous_context = false;
    for (index, line) in lines.iter().enumerate() {
        let here = shapes.context.is_match(line);
        let scrubbed = shapes.specific.replace_all(line, SECRET);
        if here || previous_context {
            out.push_str(&shapes.long_run.replace_all(&scrubbed, SECRET));
        } else {
            out.push_str(&scrubbed);
        }
        if index.saturating_add(1) < lines.len() {
            out.push('\n');
        }
        previous_context = here;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(text: &str) -> String {
        scrub_secrets(text).unwrap_or_default()
    }

    #[test]
    fn every_pattern_compiles() {
        for pattern in SPECIFIC_SHAPES {
            assert!(Regex::new(pattern).is_ok(), "{pattern}");
        }
        assert!(SHAPES.is_some());
    }

    #[test]
    fn scrubs_shapes_context_runs_and_pem_blocks() {
        let planted = "AWS_ACCESS_KEY_ID=AKIAABCDEFGHIJKLMNOP\nplain line stays\n"; // gitleaks:allow
        let out = sc(planted);
        assert!(!out.contains("AKIAABCDEFGHIJKLMNOP"), "{out}"); // gitleaks:allow
        assert!(out.contains("plain line stays"));

        let context = "api_token:\nQWxhZGRpbjpvcGVuIHNlc2FtZQQWxhZGRpbjpvcGVu\nnext\n";
        let out = sc(context);
        assert!(
            !out.contains("QWxhZGRpbjpvcGVuIHNlc2FtZQQWxhZGRpbjpvcGVu"),
            "{out}"
        );

        let hash_only = "commit 0123456789abcdef0123456789abcdef01234567\n";
        assert_eq!(
            sc(hash_only),
            hash_only,
            "a commit hash with no context stays"
        );

        let pem = "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n-----END OPENSSH PRIVATE KEY-----\nb"; // gitleaks:allow
        assert_eq!(sc(pem), "a\n[secret]\nb");
        let cut = "-----BEGIN RSA PRIVATE KEY-----\nAAAA\nBBBB";
        assert_eq!(sc(cut), "[secret]");

        let gh = format!("x ghp_{} y", "a".repeat(36));
        assert_eq!(sc(&gh), "x [secret] y");
    }
}
