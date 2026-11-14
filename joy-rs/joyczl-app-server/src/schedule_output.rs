//! Materialize committed schedule outputs without rerunning their model turns.
use joyczl_state::schedule_completions::ScheduleCompletions;
use std::{io::Write, path::Path};

fn write_output(home: &Path, file_name: &str, body: &str) -> std::io::Result<()> {
    let component = Path::new(file_name);
    if component.file_name() != Some(component.as_os_str()) || file_name.contains('\\') {
        return Err(std::io::Error::other(
            "Schedule output must be one file name",
        ));
    }
    let dir = home.join("outbox");
    std::fs::create_dir_all(&dir)?;
    if std::fs::symlink_metadata(&dir)?.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "Schedule outbox cannot be a symbolic link",
        ));
    }
    let path = dir.join(file_name);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {
            if std::fs::read(&path)? == body.as_bytes() {
                std::fs::File::open(&path)?.sync_all()?;
                #[cfg(unix)]
                std::fs::File::open(&dir)?.sync_all()?;
                return Ok(());
            }
            return Err(std::io::Error::other(
                "Existing output differs; retained for review",
            ));
        }
        Ok(_) => {
            return Err(std::io::Error::other(
                "Output destination is not a regular file",
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    let temporary = dir.join(format!(".schedule-{}-{nonce}.pending", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        // A hard link publishes atomically without overwriting a concurrent human edit.
        std::fs::hard_link(&temporary, &path)?;
        #[cfg(unix)]
        std::fs::File::open(&dir)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

pub(crate) async fn deliver(rows: &ScheduleCompletions, home: &Path) {
    let pending = match rows.pending_outputs().await {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("(joy) schedule output outbox unavailable: {error}");
            return;
        }
    };
    for row in pending {
        match write_output(home, &row.output_path, &row.output) {
            Ok(()) => {
                if let Err(error) = rows.output_written(&row.name, &row.minute).await {
                    eprintln!("(joy) schedule output receipt not recorded: {error}");
                }
            }
            Err(error) => eprintln!(
                "(joy) schedule '{}' output retained for retry: {error}",
                row.name
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use joyczl_state::schedule_completions::ScheduleCompletion;

    #[tokio::test]
    async fn output_retry_uses_the_committed_result_without_overwriting_human_changes() {
        let home = tempfile::tempdir().unwrap();
        let pool = joyczl_state::open(&home.path().join("state.db"))
            .await
            .unwrap();
        let schedules = joyczl_state::Schedules::new(pool.clone());
        let rows = ScheduleCompletions::new(pool);
        schedules.ensure("job", true, "outbox").await.unwrap();
        schedules.claim("job", "minute").await.unwrap();
        rows.settle(ScheduleCompletion {
            name: "job",
            minute: "minute",
            status: "produced",
            detail: "",
            output: "original report",
            preview: "report",
            output_path: Some("report.md"),
            notify_requested: false,
            notify_target: None,
        })
        .await
        .unwrap();
        std::fs::write(home.path().join("outbox"), "unavailable directory").unwrap();
        deliver(&rows, home.path()).await;
        assert!(!rows.output_delivered("job", "minute").await.unwrap());
        std::fs::remove_file(home.path().join("outbox")).unwrap();
        std::fs::create_dir(home.path().join("outbox")).unwrap();
        let path = home.path().join("outbox/report.md");
        std::fs::write(&path, "human changes").unwrap();
        deliver(&rows, home.path()).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "human changes");
        assert!(!rows.output_delivered("job", "minute").await.unwrap());
        std::fs::remove_file(&path).unwrap();
        deliver(&rows, home.path()).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original report");
        assert!(rows.output_delivered("job", "minute").await.unwrap());
        assert!(!schedules.claim_once("job", "minute").await.unwrap());
    }

    #[test]
    fn output_paths_cannot_escape_the_outbox() {
        let home = tempfile::tempdir().unwrap();
        assert!(write_output(home.path(), "../escape.md", "blocked").is_err());
        assert!(!home.path().join("escape.md").exists());
        write_output(home.path(), "report.md", "original").unwrap();
        write_output(home.path(), "report.md", "original").unwrap();
        assert!(write_output(home.path(), "report.md", "replacement").is_err());
    }
}
