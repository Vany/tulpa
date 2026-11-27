//! The deny list (design §2). It wins over every root.

use std::path::{Component, Path};

/// Directories under the home directory that are never touched.
const HOME_DIRS: &[&str] = &[
    ".ssh",
    "library",
    ".aws",
    ".kube",
    ".docker",
    ".gnupg",
    ".mozilla",
    ".config/gh",
    ".config/tulpa",
    ".password-store",
];
/// Directories anywhere that are never touched.
const ANY_DIRS: &[&str] = &[".ssh", ".aws", ".kube", ".docker", ".gnupg"];
/// Exact file names, anywhere.
const NAMES: &[&str] = &[
    ".git-credentials",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "keychain-2.db",
];
/// File name suffixes, anywhere. `_history` covers the shell and REPL
/// histories (`.zsh_history`, `.bash_history`, `fish_history`, `.python_history`…).
const SUFFIXES: &[&str] = &[
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".keychain",
    ".keychain-db",
    ".kdbx",
    "_history",
];
/// Name prefixes, anywhere.
const PREFIXES: &[&str] = &[".env", "id_"];
/// Name substrings, anywhere.
const CONTAINS: &[&str] = &["credentials"];

/// True when `path` (absolute, real) is on the deny list. Names compare
/// case-insensitively: the Mac's volume does.
#[must_use]
pub fn is_denied(path: &Path, home: &Path) -> bool {
    let lower = |s: &str| s.to_lowercase();
    if let Ok(rest) = path.strip_prefix(home) {
        let rest = lower(&rest.to_string_lossy());
        if HOME_DIRS
            .iter()
            .any(|d| rest == *d || rest.starts_with(&format!("{d}/")))
        {
            return true;
        }
    }
    let full = lower(&path.to_string_lossy());
    if full.starts_with("/library/keychains") || full.starts_with("/private/var/db") {
        return true;
    }
    let names: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(lower(&name.to_string_lossy())),
            _ => None,
        })
        .collect();
    let mut previous: Option<&str> = None;
    for name in &names {
        let name = name.as_str();
        if ANY_DIRS.contains(&name)
            || NAMES.contains(&name)
            || PREFIXES.iter().any(|p| name.starts_with(p))
            || CONTAINS.iter().any(|c| name.contains(c))
            || SUFFIXES.iter().any(|s| name.ends_with(s))
            || (previous == Some(".git") && name == "config")
        {
            return true;
        }
        previous = Some(name);
    }
    false
}

/// The deny list as git exclude pathspecs for a repo at `repo`.
///
/// Derived from the same constants `is_denied` reads, so `git.diff` can never
/// show what `read` refuses. Home directories are excluded only where they fall inside
/// the repo (a dotfiles repo at `~`); everything else matches at any depth.
#[must_use]
pub fn git_excludes(repo: &Path, home: &Path) -> Vec<String> {
    let spec = |pattern: String| format!(":(exclude,glob,icase){pattern}");
    let mut out = Vec::new();
    for dir in HOME_DIRS {
        if let Ok(rel) = home.join(dir).strip_prefix(repo) {
            out.push(spec(format!("{}/**", rel.display())));
        }
    }
    out.extend(ANY_DIRS.iter().map(|d| spec(format!("**/{d}/**"))));
    out.extend(NAMES.iter().map(|n| spec(format!("**/{n}"))));
    out.extend(SUFFIXES.iter().map(|s| spec(format!("**/*{s}"))));
    out.extend(PREFIXES.iter().map(|p| spec(format!("**/{p}*"))));
    out.extend(CONTAINS.iter().map(|c| spec(format!("**/*{c}*"))));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_the_listed_shapes_and_keeps_ordinary_files() {
        let home = Path::new("/home/u");
        for bad in [
            "/home/u/.ssh/id_ed25519",
            "/home/u/.SSH/config",
            "/home/u/Library/Keychains/login.keychain-db",
            "/home/u/work/r/.env",
            "/home/u/work/r/.env.local",
            "/home/u/work/r/certs/server.pem",
            "/home/u/work/r/tls.key",
            "/home/u/work/r/.git/config",
            "/home/u/work/r/.git-credentials",
            "/home/u/work/r/app.credentials.json",
            "/home/u/work/r/deploy/id_rsa",
            "/home/u/work/r/.docker/config.json",
            "/home/u/.aws/credentials",
            "/home/u/.config/tulpa/token",
            "/home/u/.zsh_history",
            "/home/u/work/r/.bash_history",
            "/home/u/.local/share/fish/fish_history",
            "/home/u/keystore.kdbx",
            "/Library/Keychains/System.keychain",
        ] {
            assert!(is_denied(Path::new(bad), home), "{bad} must be denied");
        }
        for good in [
            "/home/u/work/r/src/main.rs",
            "/home/u/work/r/.git/HEAD",
            "/home/u/work/r/environment.md",
            "/home/u/work/r/keyboard.rs",
            "/home/u/work/r/Library.md",
        ] {
            assert!(!is_denied(Path::new(good), home), "{good} must pass");
        }
    }

    #[test]
    fn git_excludes_cover_every_constant() {
        let home = Path::new("/home/u");
        let at_home = git_excludes(home, home).join("\n");
        for item in HOME_DIRS
            .iter()
            .chain(ANY_DIRS)
            .chain(NAMES)
            .chain(SUFFIXES)
            .chain(PREFIXES)
            .chain(CONTAINS)
        {
            assert!(at_home.contains(item), "{item} missing from {at_home}");
        }
        let elsewhere = git_excludes(Path::new("/home/u/work/r"), home).join("\n");
        assert!(
            !elsewhere.contains("library/**"),
            "home dirs leak into an unrelated repo: {elsewhere}"
        );
    }
}
