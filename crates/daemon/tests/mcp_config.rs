use bondsymphonic_daemon::mcp::McpManager;
use bondsymphonic_proto::*;

#[test]
fn credential_arguments_are_rejected_before_preview_or_persistence() {
    for args in [
        vec!["--token", "SECRET_SENTINEL"],
        vec!["--api-key=SECRET_SENTINEL"],
        vec!["password=SECRET_SENTINEL"],
        vec!["Bearer SECRET_SENTINEL"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let manager = McpManager::load(root.path().to_owned()).unwrap();
        let params = McpImportParams {
            document: McpSecretValue(
                serde_json::json!({"mcpServers":{"test":{"command":"node","args":args}}})
                    .to_string(),
            ),
            resolutions: Default::default(),
            expected_revision: None,
        };
        let preview = manager.import_preview(&params).unwrap();
        assert!(preview.servers.is_empty());
        assert!(!format!("{preview:?}").contains("SECRET_SENTINEL"));
        assert!(manager.import_apply(params).is_err());
        let mut server = secret_server("test");
        server.transport = McpTransportConfig::Stdio {
            command: "node".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: Default::default(),
            env_secrets: Default::default(),
        };
        let error = manager
            .upsert(McpUpsertParams {
                server,
                secrets: vec![],
                expected_revision: None,
            })
            .unwrap_err();
        assert!(!format!("{error:?}").contains("SECRET_SENTINEL"));
        assert!(!manager.export().unwrap().contains("SECRET_SENTINEL"));
    }
}

#[test]
fn reload_rejects_cross_server_and_duplicate_secret_references() {
    for stored in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let manager = McpManager::load(root.path().to_owned()).unwrap();
        manager
            .upsert(McpUpsertParams {
                server: secret_server("first"),
                secrets: if stored {
                    vec![McpSecretWrite::Set {
                        reference: "test-token".into(),
                        value: McpSecretValue("SECRET_SENTINEL".into()),
                    }]
                } else {
                    vec![]
                },
                expected_revision: None,
            })
            .unwrap();
        let path = root.path().join("mcp/registry.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["servers"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(secret_server("second")).unwrap());
        // With no duplicate configured reference, a stored value still may not
        // belong to a different config owner.
        if stored {
            manifest["servers"][1]["transport"]["auth"]["token"] = "another-token".into();
        }
        std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(McpManager::load(root.path().to_owned()).is_err());
    }
}

#[test]
fn reload_removes_only_own_private_crash_temporaries() {
    let root = tempfile::tempdir().unwrap();
    McpManager::load(root.path().to_owned()).unwrap();
    let dir = root.path().join("mcp");
    let stale = dir.join(".secrets-1-deadbeef.123.4.tmp");
    let unrelated = dir.join(".credentials.123.4.tmp");
    let misleading = dir.join(".secrets-important.tmp");
    for path in [&stale, &unrelated, &misleading] {
        std::fs::write(path, b"SECRET_SENTINEL").unwrap();
    }
    McpManager::load(root.path().to_owned()).unwrap();
    assert!(!stale.exists());
    assert!(unrelated.exists());
    assert!(misleading.exists());
}

#[cfg(windows)]
#[test]
fn windows_private_files_have_protected_restrictive_dacl() {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt};
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetFileSecurityW(
            path: *const u16,
            info: u32,
            descriptor: *mut c_void,
            length: u32,
            needed: *mut u32,
        ) -> i32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor: *const c_void,
            revision: u32,
            info: u32,
            output: *mut *mut u16,
            length: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    let root = tempfile::tempdir().unwrap();
    McpManager::load(root.path().to_owned()).unwrap();
    let path: Vec<u16> = root
        .path()
        .join("mcp/secrets-0")
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        let mut needed = 0;
        GetFileSecurityW(path.as_ptr(), 4, std::ptr::null_mut(), 0, &mut needed);
        assert!(needed > 0);
        let mut descriptor = vec![0u32; (needed as usize).div_ceil(4)];
        assert_ne!(
            GetFileSecurityW(
                path.as_ptr(),
                4,
                descriptor.as_mut_ptr().cast(),
                (descriptor.len() * 4) as u32,
                &mut needed
            ),
            0
        );
        let mut sddl = std::ptr::null_mut();
        let mut length = 0;
        assert_ne!(
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.as_ptr().cast(),
                1,
                4,
                &mut sddl,
                &mut length
            ),
            0
        );
        let mut actual_length = 0;
        while *sddl.add(actual_length) != 0 {
            actual_length += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(sddl, actual_length));
        LocalFree(sddl.cast());
        assert!(
            text.starts_with("D:P"),
            "DACL must block inheritance: {text}"
        );
        assert_eq!(
            text.matches("(A;;FA;;;").count(),
            3,
            "expected only user, SYSTEM, Administrators: {text}"
        );
        assert!(text.contains(";;;SY)") && text.contains(";;;BA)"));
        assert!(!text.contains(";;;WD)") && !text.contains(";;;BU)") && !text.contains(";;;AU)"));
    }
}

#[test]
fn preset_removed_stays_removed() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpManager::load(root.path().to_owned()).unwrap();
    let preset = manager.snapshot().servers[0].clone();
    assert!(serde_json::to_string(&preset)
        .unwrap()
        .contains("https://mcp.atlassian.com/v2/mcp"));
    manager.remove(&preset.id, Some(0)).unwrap();
    assert!(McpManager::load(root.path().to_owned())
        .unwrap()
        .snapshot()
        .servers
        .is_empty());
}

#[test]
fn revision_conflict_preserves_config() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpManager::load(root.path().to_owned()).unwrap();
    let before = manager.snapshot();
    let mut server = before.servers[0].clone();
    server.name = "changed".into();
    assert!(manager
        .upsert(McpUpsertParams {
            server,
            secrets: vec![],
            expected_revision: Some(99)
        })
        .is_err());
    assert_eq!(before, manager.snapshot());
}

#[test]
fn import_requires_collision_decision() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpManager::load(root.path().to_owned()).unwrap();
    let params = McpImportParams {
        document: McpSecretValue(r#"{"mcpServers":{"jira":{"command":"node"}}}"#.into()),
        resolutions: Default::default(),
        expected_revision: None,
    };
    assert_eq!(manager.import_preview(&params).unwrap().collisions.len(), 1);
    assert!(manager.import_apply(params).is_err());
    let params = McpImportParams {
        document: McpSecretValue(r#"{"mcpServers":{"jira":{"command":"node"}}}"#.into()),
        resolutions: [("jira".into(), "jira-copy".into())].into(),
        expected_revision: Some(0),
    };
    let saved = manager.import_apply(params).unwrap();
    assert_eq!(saved.servers.len(), 2);
    assert_eq!(
        McpManager::load(root.path().to_owned()).unwrap().snapshot(),
        saved
    );
}

#[test]
fn export_omits_secret_values() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpManager::load(root.path().to_owned()).unwrap();
    let params = McpImportParams {
        document: McpSecretValue(
            r#"{"mcpServers":{"test":{"command":"node","env":{"TOKEN":"SECRET_SENTINEL"}}}}"#
                .into(),
        ),
        resolutions: Default::default(),
        expected_revision: None,
    };
    assert!(!format!("{:?}", manager.import_preview(&params).unwrap()).contains("SECRET_SENTINEL"));
    manager.import_apply(params).unwrap();
    assert!(!manager.export().unwrap().contains("SECRET_SENTINEL"));
    assert!(
        !std::fs::read_to_string(root.path().join("mcp/registry.json"))
            .unwrap()
            .contains("SECRET_SENTINEL")
    );
}

#[test]
fn workspace_override_inherits() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpManager::load(root.path().to_owned()).unwrap();
    let workspace = WorkspaceId("ws_test".into());
    let id = manager.snapshot().servers[0].id.clone();
    manager
        .set_override(
            McpOverrideParams {
                server_id: id.clone(),
                workspace_id: workspace.clone(),
                value: McpWorkspaceOverride::Disabled,
            },
            None,
        )
        .unwrap();
    assert!(manager.resolve(workspace.clone(), "claude").is_empty());
    manager
        .set_override(
            McpOverrideParams {
                server_id: id,
                workspace_id: workspace.clone(),
                value: McpWorkspaceOverride::Inherit,
            },
            None,
        )
        .unwrap();
    assert_eq!(manager.resolve(workspace, "claude").len(), 1);
}

fn secret_server(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.into(),
        name: id.into(),
        transport: McpTransportConfig::Http {
            url: "https://example.com/mcp".into(),
            auth: McpAuthConfig::Bearer {
                token: "test-token".into(),
            },
        },
        enabled: true,
        default_workspace_enabled: true,
        enabled_backends: vec!["claude".into()],
    }
}

#[test]
fn secrets_are_private_owned_and_removed_with_server() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpManager::load(root.path().to_owned()).unwrap();
    let params = McpUpsertParams {
        server: secret_server("test"),
        secrets: vec![McpSecretWrite::Set {
            reference: "test-token".into(),
            value: McpSecretValue("SECRET_SENTINEL".into()),
        }],
        expected_revision: Some(0),
    };
    assert!(!format!("{params:?}").contains("SECRET_SENTINEL"));
    manager.upsert(params).unwrap();
    assert!(manager
        .upsert(McpUpsertParams {
            server: secret_server("other"),
            secrets: vec![],
            expected_revision: None
        })
        .is_err());
    manager
        .upsert(McpUpsertParams {
            server: secret_server("test"),
            secrets: vec![McpSecretWrite::Set {
                reference: "test-token".into(),
                value: McpSecretValue(" ".into()),
            }],
            expected_revision: None,
        })
        .unwrap();
    let files: Vec<_> = std::fs::read_dir(root.path().join("mcp"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let private = files
        .iter()
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("secrets-")
        })
        .unwrap();
    assert!(std::fs::read_to_string(private)
        .unwrap()
        .contains("SECRET_SENTINEL"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(private).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    manager.remove(&"test".into(), None).unwrap();
    for entry in std::fs::read_dir(root.path().join("mcp")).unwrap() {
        assert!(!std::fs::read_to_string(entry.unwrap().path())
            .unwrap()
            .contains("SECRET_SENTINEL"));
    }
}

#[test]
fn invalid_update_never_persists_partial_secrets() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpManager::load(root.path().to_owned()).unwrap();
    let before = manager.snapshot();
    let mut server = secret_server("test");
    server.id = "../escape".into();
    let error = manager
        .upsert(McpUpsertParams {
            server,
            secrets: vec![McpSecretWrite::Set {
                reference: "test-token".into(),
                value: McpSecretValue("SECRET_SENTINEL".into()),
            }],
            expected_revision: None,
        })
        .unwrap_err();
    assert!(!format!("{error:?}").contains("SECRET_SENTINEL"));
    assert_eq!(before, manager.snapshot());
    for entry in std::fs::read_dir(root.path().join("mcp")).unwrap() {
        assert!(!std::fs::read_to_string(entry.unwrap().path())
            .unwrap()
            .contains("SECRET_SENTINEL"));
    }
}

#[test]
fn failed_registry_write_preserves_published_state() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpManager::load(root.path().to_owned()).unwrap();
    let before = manager.snapshot();
    let path = root.path().join("mcp/registry.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(manager
        .upsert(McpUpsertParams {
            server: secret_server("test"),
            secrets: vec![],
            expected_revision: None
        })
        .is_err());
    assert_eq!(before, manager.snapshot());
    assert_eq!(
        std::fs::read_dir(root.path().join("mcp")).unwrap().count(),
        2
    );
}

#[test]
fn import_rejects_sse_and_unknown_fields_without_echoing_values() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpManager::load(root.path().to_owned()).unwrap();
    let params = McpImportParams { document: McpSecretValue(r#"{"mcpServers":{"SECRET_SENTINEL":{"type":"sse","url":"https://example.com/mcp","password":"SECRET_SENTINEL"}}}"#.into()), resolutions: Default::default(), expected_revision: None };
    let preview = manager.import_preview(&params).unwrap();
    assert_eq!(preview.issues.len(), 1);
    assert!(!format!("{preview:?}").contains("SECRET_SENTINEL"));
    assert!(manager.import_apply(params).is_err());
}

#[test]
fn explicit_secret_remove_clears_value_but_preserves_reference() {
    let root = tempfile::tempdir().unwrap();
    let manager = McpManager::load(root.path().to_owned()).unwrap();
    manager
        .upsert(McpUpsertParams {
            server: secret_server("test"),
            secrets: vec![McpSecretWrite::Set {
                reference: "test-token".into(),
                value: McpSecretValue("SECRET_SENTINEL".into()),
            }],
            expected_revision: None,
        })
        .unwrap();
    manager
        .upsert(McpUpsertParams {
            server: secret_server("test"),
            secrets: vec![McpSecretWrite::Remove {
                reference: "test-token".into(),
            }],
            expected_revision: None,
        })
        .unwrap();
    assert!(manager.export().unwrap().contains("test-token"));
    for entry in std::fs::read_dir(root.path().join("mcp")).unwrap() {
        assert!(!std::fs::read_to_string(entry.unwrap().path())
            .unwrap()
            .contains("SECRET_SENTINEL"));
    }
}
