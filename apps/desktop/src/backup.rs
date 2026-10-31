//! The purge's safety rule: it deletes the remote's history only when the photos were backed up recently. The backup tool is
//! restic (or anything answering `snapshots --latest 1 --json` alike), named in the config as `photo_backup`.

use std::time::SystemTime;

use crate::clock::parse_rfc3339;

/// Hours since the newest snapshot in `restic snapshots --latest 1 --json` output (a list of snapshots, or of groups that
/// each hold `snapshots`).
pub fn newest_age_hours(json: &str, now: SystemTime) -> Result<f64, String> {
    let data: serde_json::Value = serde_json::from_str(json).map_err(|_| "`photo_backup snapshots` gave unparseable output".to_string())?;
    let mut snaps: Vec<&serde_json::Value> = Vec::new();
    for g in data.as_array().ok_or("`photo_backup snapshots` gave unparseable output")? {
        match g.get("snapshots").and_then(|s| s.as_array()) {
            Some(inner) => snaps.extend(inner),
            None => snaps.push(g),
        }
    }
    let newest = snaps
        .iter()
        .map(|s| s.get("time").and_then(|t| t.as_str()).and_then(parse_rfc3339).ok_or_else(|| "a snapshot without a readable time".to_string()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .ok_or("no restic snapshots found")?;
    Ok(now.duration_since(newest).map_or(0.0, |d| d.as_secs_f64() / 3600.0))
}

/// Run `<command> snapshots --latest 1 --json` and say how old the newest snapshot is.
pub fn latest_age_hours(command: &str, now: SystemTime) -> Result<f64, String> {
    let out = std::process::Command::new(command)
        .args(["snapshots", "--latest", "1", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run `{command}` to confirm a recent backup: {e}"))?;
    if !out.status.success() {
        return Err("`photo_backup snapshots` failed; cannot confirm a recent backup".into());
    }
    newest_age_hours(&String::from_utf8_lossy(&out.stdout), now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> SystemTime {
        parse_rfc3339("2026-11-01T12:00:00Z").unwrap()
    }

    #[test]
    fn the_age_is_the_newest_snapshots() {
        let flat = r#"[{"time":"2026-11-01T10:00:00.123456789+00:00","id":"a"},{"time":"2026-10-30T10:00:00Z"}]"#;
        assert!((newest_age_hours(flat, now()).unwrap() - 2.0).abs() < 1e-3);
        let grouped =
            r#"[{"group_key":{},"snapshots":[{"time":"2026-11-01T06:00:00+02:00"}]},{"group_key":{},"snapshots":[{"time":"2026-10-01T00:00:00Z"}]}]"#;
        assert!((newest_age_hours(grouped, now()).unwrap() - 8.0).abs() < 1e-6);
    }

    #[test]
    fn nothing_readable_is_an_error_not_a_pass() {
        assert!(newest_age_hours("[]", now()).unwrap_err().contains("no restic snapshots"));
        assert!(newest_age_hours("nope", now()).unwrap_err().contains("unparseable"));
        assert!(newest_age_hours("{}", now()).unwrap_err().contains("unparseable"));
        assert!(newest_age_hours(r#"[{"id":"x"}]"#, now()).unwrap_err().contains("time"));
        assert!(latest_age_hours("/nonexistent/restic", now()).unwrap_err().contains("cannot run"));
    }

    #[test]
    fn a_command_that_fails_cannot_confirm_a_backup() {
        let d = std::env::temp_dir().join(format!("lc-desktop-backup-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for (name, body, want) in
            [("fail.sh", "#!/bin/sh\nexit 3\n", "failed"), ("ok.sh", "#!/bin/sh\necho '[{\"time\":\"2026-11-01T11:00:00Z\"}]'\n", "")]
        {
            let f = d.join(name);
            std::fs::write(&f, body).unwrap();
            std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            let r = latest_age_hours(&f.to_string_lossy(), now());
            if want.is_empty() {
                assert!((r.unwrap() - 1.0).abs() < 1e-6);
            } else {
                assert!(r.unwrap_err().contains(want));
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
