//! The device-job source of a SERVER-mode service (#130): the Codex
//! server's bridge has no till, so it runs the `file.write` executor ONLY.
//! `devbridge-service`'s `run_server` asks [`validate_server_pokladna_config`]
//! and builds [`server_registry_from_config`]; an invalid block is an ERROR
//! log and the source stays off — never a start failure of the print server
//! every store depends on.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use devbridge_core::config::PokladnaClientConfig;

use super::executor::ExecutorRegistry;
use super::file_write::FileWriteExecutor;
use super::journal::Journal;
use crate::startup_validation::{channel_problems, file_write_problems};

/// The file executor of `[client.pokladna.file_write]` (#130) — a real
/// folder in every mode (the test executor does not stand in for it), staged
/// in `data_dir`.
pub(crate) fn register_file_write(
    registry: &mut ExecutorRegistry,
    cfg: &PokladnaClientConfig,
    data_dir: &Path,
    journal: &Arc<Journal>,
) -> Result<()> {
    if let Some(file_write) = &cfg.file_write {
        registry.register(Arc::new(
            FileWriteExecutor::new(file_write, data_dir, Arc::clone(journal))
                .context("file.write executor")?,
        ))?;
    }
    Ok(())
}

/// The executors of a SERVER-mode service: the file executor ONLY. Portos,
/// the TIM terminal, the test executor and the tills of `[client.pokladna]`
/// are never built here ([`server_ignored`] names them for the log). Fails
/// without `[client.pokladna.file_write]`.
pub fn server_registry_from_config(
    cfg: &PokladnaClientConfig,
    data_dir: &Path,
    journal: &Arc<Journal>,
) -> Result<ExecutorRegistry> {
    if cfg.file_write.is_none() {
        bail!(
            "[client.pokladna.file_write] is missing - a server-mode bridge runs file.* jobs only"
        );
    }
    let mut registry = ExecutorRegistry::default();
    register_file_write(&mut registry, cfg, data_dir, journal)?;
    Ok(registry)
}

/// `[client.pokladna]` of a SERVER-mode service (#130: the Codex server's
/// bridge). `Ok(false)` = not enabled, the source stays off. There the
/// source runs the file executor ONLY, so `[client.pokladna.file_write]` is
/// required. The tills, the TIM terminal, the test executor and the LAN API
/// are store settings the server never runs — named in [`server_ignored`].
/// The caller logs an `Err` and keeps the server running: a config mistake
/// must never stop the IPP / gRPC server of every store.
pub fn validate_server_pokladna_config(p: &PokladnaClientConfig) -> Result<bool> {
    if !p.enabled {
        return Ok(false);
    }
    let mut problems = channel_problems(p);
    if p.file_write.is_none() {
        problems.push(
            "[client.pokladna.file_write] is missing - a server-mode bridge runs file.* jobs only"
                .to_string(),
        );
    }
    problems.extend(file_write_problems(p));
    if problems.is_empty() {
        return Ok(true);
    }
    bail!(
        "[client.pokladna] of this server is enabled but invalid: {}. Suggestion: re-run the \
         installer in server mode with DEVBRIDGE_POKLADNA_URL / DEVBRIDGE_POKLADNA_API_KEY / \
         DEVBRIDGE_POKLADNA_FILE_WRITE_DIR set.",
        problems.join("; ")
    )
}

/// Store settings of `[client.pokladna]` a SERVER-mode service ignores
/// (#130), for one WARN line. Empty = nothing ignored.
pub fn server_ignored(p: &PokladnaClientConfig) -> Vec<&'static str> {
    let mut ignored = Vec::new();
    if !p.kasy.is_empty() {
        ignored.push("[[client.pokladna.kasy]]");
    }
    if p.tim.is_some() {
        ignored.push("[client.pokladna.tim]");
    }
    if p.lan_api.enabled {
        ignored.push("[client.pokladna.lan_api]");
    }
    if p.test_executor {
        ignored.push("test_executor");
    }
    if p.fiscal {
        ignored.push("fiscal (no Portos on a server)");
    }
    ignored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_jobs::executor::DeviceKind;
    use crate::device_jobs::registry_from_config;

    fn server_pokladna() -> devbridge_core::config::PokladnaClientConfig {
        devbridge_core::config::PokladnaClientConfig {
            enabled: true,
            url: "https://erp.example.test".into(),
            api_key: "pokladna-secret-key".into(),
            fiscal: false,
            file_write: Some(devbridge_core::config::FileWriteConfig {
                target_dir: "C:\\ORION\\COMMUNICATOR\\data\\in".into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_validate_server_pokladna_config() {
        assert!(
            !validate_server_pokladna_config(&Default::default()).unwrap(),
            "disabled = off"
        );
        let mut off = server_pokladna();
        off.enabled = false;
        off.url = "garbage".into();
        assert!(
            !validate_server_pokladna_config(&off).unwrap(),
            "disabled is never checked"
        );
        assert!(validate_server_pokladna_config(&server_pokladna()).unwrap());

        let mut no_folder = server_pokladna();
        no_folder.file_write = None;
        let msg = validate_server_pokladna_config(&no_folder)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("[client.pokladna.file_write] is missing"),
            "{msg}"
        );

        let mut bad = server_pokladna();
        bad.url = "erp.example.test".into();
        bad.api_key = " ".into();
        bad.wait_s = 56;
        bad.heartbeat_interval_secs = 0;
        bad.file_write = Some(devbridge_core::config::FileWriteConfig {
            target_dir: "\\\\nas\\in".into(),
        });
        let msg = validate_server_pokladna_config(&bad)
            .unwrap_err()
            .to_string();
        for want in [
            "must be an http(s):// base URL",
            "api_key is empty",
            "wait_s = 56",
            "heartbeat_interval_secs must be >= 1",
            "file_write.target_dir must be a local folder",
            "DEVBRIDGE_POKLADNA_FILE_WRITE_DIR",
        ] {
            assert!(msg.contains(want), "{want}: {msg}");
        }
        assert!(!msg.contains("pokladna-secret-key"), "{msg}");
    }

    #[test]
    fn test_server_ignored_names_every_store_setting() {
        assert!(server_ignored(&server_pokladna()).is_empty());
        let mut store = server_pokladna();
        store.fiscal = true;
        store.test_executor = true;
        store.lan_api.enabled = true;
        store.tim = Some(devbridge_core::config::TimConfig {
            library_path: "C:\\TimApi\\timapi.dll".into(),
            integrator_id: devbridge_core::config::TIM_DEFAULT_INTEGRATOR_ID.into(),
            commit_timeout_s: 60,
            rollback_deadline_s: 45,
        });
        store.kasy = vec![devbridge_core::config::PokladnaKasaConfig {
            id: 3,
            portos_url: None,
            terminal_ip: None,
            terminal_tid: None,
            lan_key: None,
        }];
        assert_eq!(
            server_ignored(&store),
            vec![
                "[[client.pokladna.kasy]]",
                "[client.pokladna.tim]",
                "[client.pokladna.lan_api]",
                "test_executor",
                "fiscal (no Portos on a server)",
            ]
        );
    }

    #[test]
    fn test_registry_adds_the_file_executor_in_every_mode() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open_in_memory().unwrap());
        let file_write = Some(devbridge_core::config::FileWriteConfig {
            target_dir: dir.path().display().to_string(),
        });
        // a store with Portos AND a file folder (#130)
        let store = PokladnaClientConfig {
            file_write: file_write.clone(),
            ..Default::default()
        };
        let with_files = registry_from_config(&store, dir.path(), &journal).unwrap();
        assert_eq!(
            with_files.kinds(),
            vec![DeviceKind::Fiscal, DeviceKind::File]
        );
        assert_eq!(with_files.get(DeviceKind::File).unwrap().name(), "file");
        assert_eq!(
            with_files.device_status(),
            "portos=unknown; terminal=none; file=ok"
        );
        // the test executor does NOT stand in for the folder: a real one runs
        let e2e = PokladnaClientConfig {
            test_executor: true,
            file_write: file_write.clone(),
            ..Default::default()
        };
        let e2e_registry = registry_from_config(&e2e, dir.path(), &journal).unwrap();
        assert_eq!(e2e_registry.get(DeviceKind::File).unwrap().name(), "file");
        assert_eq!(
            e2e_registry.device_status(),
            "portos=test; terminal=test; file=ok"
        );
        // a relative folder is a start error of the source
        let bad = PokladnaClientConfig {
            file_write: Some(devbridge_core::config::FileWriteConfig {
                target_dir: "in".into(),
            }),
            ..Default::default()
        };
        let err = registry_from_config(&bad, dir.path(), &journal)
            .unwrap_err()
            .to_string();
        assert!(err.contains("file.write executor"), "{err}");
        // server mode: the file executor only, whatever else is configured
        let server = PokladnaClientConfig {
            test_executor: true,
            file_write,
            ..Default::default()
        };
        let only_files = server_registry_from_config(&server, dir.path(), &journal).unwrap();
        assert_eq!(only_files.kinds(), vec![DeviceKind::File]);
        assert_eq!(
            only_files.device_status(),
            "portos=none; terminal=none; file=ok"
        );
        let without =
            server_registry_from_config(&PokladnaClientConfig::default(), dir.path(), &journal)
                .unwrap_err()
                .to_string();
        assert!(without.contains("file.* jobs only"), "{without}");
    }
}
