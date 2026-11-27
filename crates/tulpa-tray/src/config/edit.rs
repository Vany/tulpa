//! One setting changed in the local config file, everything else kept: the
//! menu's device pickers save their pick as `key = "<name>"` in one table.
//!
//! The edit is textual and minimal: the key's line in its table gets the new
//! value (its indentation and a trailing comment stay), or a new line right
//! under the table's header, or — no such table — a new table at the end.
//! Every other line, comment and blank line is kept byte for byte. An edit that
//! would not read back as the same config with only that value changed is
//! refused, so a pick can never leave a file the next start can't read, nor
//! change anything else in it.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// `text` with `key = "<value>"` set in `[table]`.
///
/// # Errors
///
/// The key's current value is not a one-line value (a multi-line string, an
/// array or an inline table), or the edited text does not read back as tray
/// config equal to `text`'s with only that key set to `value` (e.g. the table
/// is defined some other way); the reason names the key.
pub fn set_string(text: &str, table: &str, key: &str, value: &str) -> Result<String, String> {
    let wanted = format!("{key} = {}", quoted(value));
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut in_table = false;
    let mut header_at = None;
    let mut found = None;
    for (index, line) in lines.iter().enumerate() {
        if let Some(name) = header_of(line) {
            in_table = name == table;
            if in_table && header_at.is_none() {
                header_at = Some(index);
            }
        } else if in_table && found.is_none() && is_key_line(line, key) {
            found = Some(index);
        }
    }
    let mut out = String::with_capacity(text.len().saturating_add(wanted.len()).saturating_add(16));
    match (found, header_at) {
        (Some(at), _) => {
            for (index, line) in lines.iter().enumerate() {
                if index == at {
                    out.push_str(&replaced(line, key, &wanted)?);
                } else {
                    out.push_str(line);
                }
            }
        }
        (None, Some(at)) => {
            for (index, line) in lines.iter().enumerate() {
                out.push_str(line);
                if index == at {
                    // The new line ends like its header.
                    let eol = if line.ends_with("\r\n") { "\r\n" } else { "\n" };
                    if !line.ends_with('\n') {
                        out.push_str(eol);
                    }
                    out.push_str(&wanted);
                    out.push_str(eol);
                }
            }
        }
        (None, None) => {
            let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
            out.push_str(text);
            if !text.is_empty() && !text.ends_with('\n') {
                out.push_str(eol);
            }
            if !text.is_empty() {
                out.push_str(eol);
            }
            let _written: std::fmt::Result = write!(out, "[{table}]{eol}{wanted}{eol}");
        }
    }
    reads_back(text, &out, table, key, value)?;
    Ok(out)
}

/// [`set_string`] on the file at `path` (a symlink's target).
///
/// The new text goes to a temporary file beside it (mode 0600, then the file's
/// own permissions) that is renamed over it, so a reader finds the old file or
/// the new one.
///
/// # Errors
///
/// The file can't be read or replaced (it then stays as it was), or
/// [`set_string`] refuses the edit.
pub fn set_string_in_file(path: &Path, table: &str, key: &str, value: &str) -> Result<(), String> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = fs::read_to_string(&target).map_err(|e| format!("{}: {e}", target.display()))?;
    let edited = set_string(&text, table, key, value)?;
    if edited == text {
        return Ok(());
    }
    let mut temporary = target.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let written = write_then_rename(&temporary, &target, &edited);
    if written.is_err() {
        let _removed: io::Result<()> = fs::remove_file(&temporary);
    }
    written.map_err(|e| format!("{}: {e}", target.display()))
}

fn write_then_rename(temporary: &Path, target: &Path, text: &str) -> io::Result<()> {
    let _stale: io::Result<()> = fs::remove_file(temporary);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::set_permissions(temporary, fs::metadata(target)?.permissions())?;
    fs::rename(temporary, target)
}

/// The table a header line opens (`[name]`, spaces inside the brackets
/// ignored); an array-of-tables header gives a name no table has.
fn header_of(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("[[") {
        let name = rest.split("]]").next().unwrap_or_default().trim();
        return Some(format!("[[{name}]]"));
    }
    let rest = trimmed.strip_prefix('[')?;
    rest.split_once(']').map(|(name, _)| name.trim().to_owned())
}

/// Whether `line` sets `key` (bare key, any indentation; a comment does not).
fn is_key_line(line: &str, key: &str) -> bool {
    line.trim_start()
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// The key's line with `wanted` in place of `key = <old value>`: the
/// indentation before and whatever follows the old value (a comment, the line
/// ending) are kept.
fn replaced(line: &str, key: &str, wanted: &str) -> Result<String, String> {
    let indent_len = line.len().saturating_sub(line.trim_start().len());
    let indent = line.get(..indent_len).unwrap_or_default();
    let after_key = line.trim_start().get(key.len()..).unwrap_or_default();
    let value = after_key
        .trim_start()
        .strip_prefix('=')
        .unwrap_or_default()
        .trim_start();
    let end = value_len(value)
        .ok_or_else(|| format!("{key}: its value is not a one-line string; change it by hand"))?;
    let rest = value.get(end..).unwrap_or_default();
    Ok(format!("{indent}{wanted}{rest}"))
}

/// The length of the one-line value at the start of `value`; `None` for one
/// that may span lines or does not end on this line.
fn value_len(value: &str) -> Option<usize> {
    if value.starts_with("\"\"\"") || value.starts_with("'''") {
        return None;
    }
    if let Some(body) = value.strip_prefix('"') {
        let mut escaped = false;
        for (index, c) in body.char_indices() {
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => return Some(index.saturating_add(2)),
                '\n' | '\r' => return None,
                _ => escaped = false,
            }
        }
        return None;
    }
    if let Some(body) = value.strip_prefix('\'') {
        let close = body.find(['\'', '\n'])?;
        return body
            .get(close..)
            .is_some_and(|c| c.starts_with('\''))
            .then_some(close.saturating_add(2));
    }
    if value.starts_with(['[', '{']) {
        return None;
    }
    let bare = value.find(['#', '\r', '\n']).unwrap_or(value.len());
    Some(value.get(..bare).unwrap_or_default().trim_end().len())
}

/// `value` as a TOML basic string.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len().saturating_add(2));
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _written: std::fmt::Result = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Refuses an edit the next start could not read, or whose result is not the
/// `original` config with `[table] key = value` and nothing else changed.
fn reads_back(
    original: &str,
    edited: &str,
    table: &str,
    key: &str,
    value: &str,
) -> Result<(), String> {
    crate::config::TrayAppConfig::from_toml(edited)
        .map_err(|e| format!("{key}: the edited config would not load ({e}); not saved"))?;
    let parse = |text: &str| -> Result<toml::Table, String> {
        toml::from_str(text).map_err(|e| format!("{key}: not saved ({e})"))
    };
    let mut expected = parse(original)?;
    let Some(section) = expected
        .entry(table)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
    else {
        return Err(format!(
            "{key}: [{table}] is not a plain table in this config; not saved"
        ));
    };
    section.insert(key.to_owned(), toml::Value::String(value.to_owned()));
    if parse(edited)? == expected {
        Ok(())
    } else {
        Err(format!(
            "{key}: the edit would change more than [{table}] {key}; not saved"
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    const CONFIG: &str = "# Local config.\n\n[link]\nenabled = false # not yet\n\n[ears]\n# on | off\nmode = \"on\"\n  mic_device = \"Old Mic\"   # picked last week\nvoice = \"Example\"\n\n[ears.env]\nmic_device = \"not this one\"\n\n[voice]\nenabled = false\n";

    #[test]
    fn an_existing_key_gets_the_new_value_and_every_other_byte_stays()
    -> Result<(), Box<dyn std::error::Error>> {
        let edited = set_string(CONFIG, "ears", "mic_device", "Example USB Mic")?;
        assert_eq!(
            edited,
            CONFIG.replace(
                "  mic_device = \"Old Mic\"   # picked last week",
                "  mic_device = \"Example USB Mic\"   # picked last week"
            ),
            "indentation and the trailing comment kept, [ears.env] untouched"
        );
        let config = crate::config::TrayAppConfig::from_toml(&edited)?;
        assert_eq!(config.ears.mic_device.as_deref(), Some("Example USB Mic"));
        Ok(())
    }

    #[test]
    fn a_missing_key_goes_under_its_header_and_a_missing_table_at_the_end()
    -> Result<(), Box<dyn std::error::Error>> {
        let edited = set_string(CONFIG, "voice", "output_device", "Example Speakers")?;
        assert_eq!(
            edited,
            CONFIG.replace(
                "[voice]\n",
                "[voice]\noutput_device = \"Example Speakers\"\n"
            )
        );
        let no_voice = "[ears]\nmode = \"off\"";
        assert_eq!(
            set_string(no_voice, "voice", "output_device", "Example Speakers")?,
            "[ears]\nmode = \"off\"\n\n[voice]\noutput_device = \"Example Speakers\"\n"
        );
        assert_eq!(
            set_string("", "ears", "mic_device", "")?,
            "[ears]\nmic_device = \"\"\n",
            "an empty pick (the default) is an empty string"
        );
        Ok(())
    }

    #[test]
    fn a_commented_key_or_one_in_another_table_is_not_the_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let text = "[ears]\n# mic_device = \"Example Mic\"\nmic_device_note = \"x\"\n";
        assert_eq!(
            set_string(text, "ears", "mic_device", "Example Mic")?,
            "[ears]\nmic_device = \"Example Mic\"\n# mic_device = \"Example Mic\"\nmic_device_note = \"x\"\n"
        );
        Ok(())
    }

    #[test]
    fn names_are_quoted_and_crlf_lines_stay_crlf() -> Result<(), Box<dyn std::error::Error>> {
        let text = "[voice]\r\noutput_device = 'Old \"Out\"' # here\r\nenabled = false\r\n";
        let edited = set_string(text, "voice", "output_device", "Desk \"Speaker\" \\ 2")?;
        assert_eq!(
            edited,
            "[voice]\r\noutput_device = \"Desk \\\"Speaker\\\" \\\\ 2\" # here\r\nenabled = false\r\n"
        );
        let config = crate::config::TrayAppConfig::from_toml(&edited)?;
        assert_eq!(
            config.voice.output_device.as_deref(),
            Some("Desk \"Speaker\" \\ 2")
        );
        Ok(())
    }

    #[test]
    fn a_new_line_ends_like_the_file_and_nothing_else_may_change()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            set_string(
                "[voice]\r\nenabled = false\r\n",
                "voice",
                "output_device",
                "Out"
            )?,
            "[voice]\r\noutput_device = \"Out\"\r\nenabled = false\r\n"
        );
        assert_eq!(
            set_string(
                "[ears]\r\nmode = \"on\"\r\n",
                "voice",
                "output_device",
                "Out"
            )?,
            "[ears]\r\nmode = \"on\"\r\n\r\n[voice]\r\noutput_device = \"Out\"\r\n"
        );
        // A key-like line inside a multi-line string comes first: editing it would change the string.
        let text = "[ears]\nnote = \"\"\"\nmic_device = \"inside a string\"\n\"\"\"\nmic_device = \"Example Mic\"\n";
        assert!(
            set_string(text, "ears", "mic_device", "Example Mic")
                .is_err_and(|why| why.contains("more than")),
            "refused, never written"
        );
        Ok(())
    }

    #[test]
    fn edits_that_would_not_read_back_are_refused() {
        for (text, why) in [
            (
                "[ears]\nmic_device = \"\"\"\nOld\n\"\"\"\n",
                "a multi-line value",
            ),
            ("[ears]\nmic_device = [\"x\"]\n", "an array"),
            (
                "voice = { output_device = \"x\" }\n",
                "a table defined inline",
            ),
            ("[ears]\nmode = \"loud\"\n", "a config that already fails"),
        ] {
            let table = if why.contains("inline") {
                "voice"
            } else {
                "ears"
            };
            let key = if table == "voice" {
                "output_device"
            } else {
                "mic_device"
            };
            assert!(set_string(text, table, key, "Example").is_err(), "{why}");
        }
    }

    #[test]
    fn the_file_is_replaced_whole_with_its_permissions() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "tulpa-config-edit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        fs::create_dir_all(&dir)?;
        let path = dir.join("config.toml");
        fs::write(&path, CONFIG)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
        let link = dir.join("link.toml");
        std::os::unix::fs::symlink(&path, &link)?;
        set_string_in_file(&link, "ears", "mic_device", "Example USB Mic")?;
        assert!(
            fs::symlink_metadata(&link)?.file_type().is_symlink(),
            "the link stays a link"
        );
        assert!(fs::read_to_string(&path)?.contains("mic_device = \"Example USB Mic\""));
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o640);
        let left: Vec<_> = fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left.len(), 2, "no temporary file left: {left:?}");
        let before = fs::read_to_string(&path)?;
        assert!(set_string_in_file(&path, "ears", "mode", "loud").is_err());
        assert_eq!(
            fs::read_to_string(&path)?,
            before,
            "a refused edit leaves the file"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
