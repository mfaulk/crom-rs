use std::path::PathBuf;

use code_agent_core::SessionId;
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::store::{resolve_within_cwd, CheckpointError, CheckpointStore, LocalCheckpointStore};
use crate::types::{CheckpointId, FileSnapshotKind, WorkspaceRelativePath};

pub const DEFAULT_MAX_DIFF_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct DiffCheckpointRequest {
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
    pub cwd: PathBuf,
    /// Cap rendered output. When None, [`DEFAULT_MAX_DIFF_BYTES`] is used.
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointDiff {
    pub checkpoint_id: CheckpointId,
    pub file_diffs: Vec<FileDiff>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileDiff {
    Modified {
        path: WorkspaceRelativePath,
        unified_diff: String,
        truncated: bool,
    },
    Created {
        path: WorkspaceRelativePath,
        current_content_preview: String,
        truncated: bool,
    },
    Deleted {
        path: WorkspaceRelativePath,
        original_content_preview: String,
        truncated: bool,
    },
    Binary {
        path: WorkspaceRelativePath,
        change_kind: BinaryChangeKind,
    },
    Redacted {
        path: WorkspaceRelativePath,
        change_kind: BinaryChangeKind,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BinaryChangeKind {
    Modified,
    Created,
    Deleted,
}

/// Exact-match basenames whose contents we never expose in a rendered
/// diff. Matched against the workspace-relative path's basename.
const REDACTED_BASENAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development",
    ".env.production",
    ".env.test",
    ".env.example",
    "secrets.toml",
    "secrets.json",
    "secrets.yaml",
    "secrets.yml",
    "credentials.json",
    "credentials.yaml",
    "credentials.yml",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "authorized_keys",
    "known_hosts",
    "htpasswd",
    ".htpasswd",
    "service-account.json",
    "kubeconfig",
];

/// Filename suffixes whose contents are redacted regardless of basename.
/// Matched case-insensitively.
const REDACTED_SUFFIXES: &[&str] = &[
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".jks",
    ".keystore",
    ".kdbx",
    ".asc",
    ".gpg",
    ".age",
];

/// Returns true iff the workspace-relative path looks like a secret file
/// whose contents we should not embed in a diff. The set is intentionally
/// conservative — false positives only mean a `[redacted Modified]`
/// marker instead of a unified diff, which is safe.
pub fn redact_path_for_diff(path: &WorkspaceRelativePath) -> bool {
    let s = path.as_str();
    let base = s.rsplit_once('/').map(|(_, b)| b).unwrap_or(s);
    let base_lc = base.to_ascii_lowercase();
    if REDACTED_BASENAMES
        .iter()
        .any(|n| n.eq_ignore_ascii_case(base))
    {
        return true;
    }
    if base.starts_with(".env.") {
        return true;
    }
    // SSH-style private keys: redact `id_*` unless it ends with `.pub`.
    if base.starts_with("id_") && !base.ends_with(".pub") {
        return true;
    }
    if REDACTED_SUFFIXES
        .iter()
        .any(|suffix| base_lc.ends_with(suffix))
    {
        return true;
    }
    false
}

/// Outcome of attempting to read the current on-disk file for a diff.
enum CurrentRead {
    /// File contents, read in full (within the size cap).
    Bytes(Vec<u8>),
    /// File exceeds the read cap; treat like a binary change so its
    /// contents are never loaded into memory or rendered.
    TooLarge,
    /// Stat or read failed (e.g. the file vanished mid-diff); skip it.
    Skip,
}

/// Stats `absolute` and reads it only when its size is within `read_cap`.
/// Files larger than the cap are reported as [`CurrentRead::TooLarge`]
/// without ever being loaded, mirroring how binary files are surfaced.
async fn read_current_capped(absolute: &std::path::Path, read_cap: u64) -> CurrentRead {
    match tokio::fs::metadata(absolute).await {
        Ok(meta) if meta.len() > read_cap => CurrentRead::TooLarge,
        Ok(_) => match tokio::fs::read(absolute).await {
            Ok(b) => CurrentRead::Bytes(b),
            Err(_) => CurrentRead::Skip,
        },
        Err(_) => CurrentRead::Skip,
    }
}

pub(crate) async fn diff_checkpoint(
    store: &LocalCheckpointStore,
    request: DiffCheckpointRequest,
) -> Result<CheckpointDiff, CheckpointError> {
    // We allow callers to supply the session id directly to avoid a scan.
    // If the manifest under that session doesn't have the checkpoint we
    // fall back to the directory walk.
    let session_id = if let Ok(meta) = store
        .get_checkpoint(&request.session_id, &request.checkpoint_id)
        .await
    {
        let _ = meta;
        request.session_id.clone()
    } else {
        store
            .find_session_for_checkpoint(&request.checkpoint_id)
            .await?
    };
    let meta = store
        .get_checkpoint(&session_id, &request.checkpoint_id)
        .await?;

    let cap = request.max_bytes.unwrap_or(DEFAULT_MAX_DIFF_BYTES);
    // Bound how much of the *current* on-disk file we will read. The blob
    // side was capped at capture time, but the live file may have grown
    // unbounded since then; reading it whole would defeat `max_bytes`,
    // which only limits rendered output. Reuse the per-file capture cap.
    let read_cap = store.limits().max_checkpoint_bytes_per_file;
    let mut total_bytes = 0usize;
    let mut truncated_overall = false;
    let mut file_diffs: Vec<FileDiff> = Vec::new();

    for snap in &meta.files_tracked {
        let path = snap.path();
        let absolute = match resolve_within_cwd(&request.cwd, &path.to_path_buf()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let exists_now = absolute.exists();
        let redact = redact_path_for_diff(path);

        match snap {
            FileSnapshotKind::ExistingFile { content_hash, .. } => {
                let original = match store.read_blob(&session_id, content_hash.as_str()).await {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                if !exists_now {
                    if redact {
                        file_diffs.push(FileDiff::Redacted {
                            path: path.clone(),
                            change_kind: BinaryChangeKind::Deleted,
                        });
                        continue;
                    }
                    if !is_text(&original) {
                        file_diffs.push(FileDiff::Binary {
                            path: path.clone(),
                            change_kind: BinaryChangeKind::Deleted,
                        });
                        continue;
                    }
                    let (preview, t) = preview_string(&original, cap);
                    total_bytes += preview.len();
                    file_diffs.push(FileDiff::Deleted {
                        path: path.clone(),
                        original_content_preview: preview,
                        truncated: t,
                    });
                } else {
                    let current = match read_current_capped(&absolute, read_cap).await {
                        CurrentRead::Bytes(b) => b,
                        CurrentRead::Skip => continue,
                        CurrentRead::TooLarge => {
                            if redact {
                                file_diffs.push(FileDiff::Redacted {
                                    path: path.clone(),
                                    change_kind: BinaryChangeKind::Modified,
                                });
                            } else {
                                file_diffs.push(FileDiff::Binary {
                                    path: path.clone(),
                                    change_kind: BinaryChangeKind::Modified,
                                });
                            }
                            continue;
                        }
                    };
                    if redact {
                        if original == current {
                            continue;
                        }
                        file_diffs.push(FileDiff::Redacted {
                            path: path.clone(),
                            change_kind: BinaryChangeKind::Modified,
                        });
                        continue;
                    }
                    if !is_text(&original) || !is_text(&current) {
                        if original != current {
                            file_diffs.push(FileDiff::Binary {
                                path: path.clone(),
                                change_kind: BinaryChangeKind::Modified,
                            });
                        }
                        continue;
                    }
                    let original_s = String::from_utf8_lossy(&original);
                    let current_s = String::from_utf8_lossy(&current);
                    if original_s == current_s {
                        continue;
                    }
                    let (rendered, t) = render_unified_diff(
                        path.as_str(),
                        original_s.as_ref(),
                        current_s.as_ref(),
                        cap.saturating_sub(total_bytes).max(1024),
                    );
                    total_bytes += rendered.len();
                    file_diffs.push(FileDiff::Modified {
                        path: path.clone(),
                        unified_diff: rendered,
                        truncated: t,
                    });
                }
            }
            FileSnapshotKind::MissingBeforeChange { .. } => {
                if !exists_now {
                    continue;
                }
                let current = match read_current_capped(&absolute, read_cap).await {
                    CurrentRead::Bytes(b) => b,
                    CurrentRead::Skip => continue,
                    CurrentRead::TooLarge => {
                        if redact {
                            file_diffs.push(FileDiff::Redacted {
                                path: path.clone(),
                                change_kind: BinaryChangeKind::Created,
                            });
                        } else {
                            file_diffs.push(FileDiff::Binary {
                                path: path.clone(),
                                change_kind: BinaryChangeKind::Created,
                            });
                        }
                        continue;
                    }
                };
                if redact {
                    file_diffs.push(FileDiff::Redacted {
                        path: path.clone(),
                        change_kind: BinaryChangeKind::Created,
                    });
                    continue;
                }
                if !is_text(&current) {
                    file_diffs.push(FileDiff::Binary {
                        path: path.clone(),
                        change_kind: BinaryChangeKind::Created,
                    });
                    continue;
                }
                let (preview, t) =
                    preview_string(&current, cap.saturating_sub(total_bytes).max(1024));
                total_bytes += preview.len();
                file_diffs.push(FileDiff::Created {
                    path: path.clone(),
                    current_content_preview: preview,
                    truncated: t,
                });
            }
        }

        if total_bytes >= cap {
            truncated_overall = true;
            break;
        }
    }

    Ok(CheckpointDiff {
        checkpoint_id: request.checkpoint_id,
        file_diffs,
        truncated: truncated_overall,
    })
}

fn render_unified_diff(path: &str, before: &str, after: &str, max_bytes: usize) -> (String, bool) {
    let diff = TextDiff::from_lines(before, after);
    let unified = diff
        .unified_diff()
        .header(&format!("checkpoint/{path}"), &format!("current/{path}"))
        .to_string();
    if unified.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !unified.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = unified[..end].to_string();
        out.push_str("\n... (diff truncated)\n");
        (out, true)
    } else {
        (unified, false)
    }
}

fn preview_string(bytes: &[u8], max_bytes: usize) -> (String, bool) {
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= max_bytes {
        (s.into_owned(), false)
    } else {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = s[..end].to_string();
        out.push_str("\n... (truncated)\n");
        (out, true)
    }
}

/// Coarse text-vs-binary heuristic: a file is "text" if it has no NUL bytes
/// in its first 8 KiB.
pub fn is_text(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8 * 1024)];
    !head.contains(&0u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Diff redaction matches `.env` and friends.
    fn redacts_env_files() {
        assert!(redact_path_for_diff(&WorkspaceRelativePath(".env".into())));
        assert!(redact_path_for_diff(&WorkspaceRelativePath(
            ".env.local".into()
        )));
        assert!(redact_path_for_diff(&WorkspaceRelativePath(
            "config/.env".into()
        )));
        assert!(!redact_path_for_diff(&WorkspaceRelativePath(
            "src/main.rs".into()
        )));
    }

    #[test]
    // Private key extensions, SSH keys, and YAML secret files are redacted.
    fn redacts_keys_and_yaml_secrets() {
        for p in [
            "tls.pem",
            "server.key",
            "deploy.p12",
            "vault.kdbx",
            "config/secrets.yaml",
            "config/credentials.yml",
            ".ssh/id_rsa",
            ".ssh/id_ed25519",
            "service-account.json",
            "kubeconfig",
            "data/Secrets.YAML",
        ] {
            assert!(
                redact_path_for_diff(&WorkspaceRelativePath(p.into())),
                "expected {p:?} to be redacted"
            );
        }
        // Public keys and source files are NOT redacted.
        for p in ["src/main.rs", ".ssh/id_rsa.pub", "README.md"] {
            assert!(
                !redact_path_for_diff(&WorkspaceRelativePath(p.into())),
                "did not expect {p:?} to be redacted"
            );
        }
    }

    #[test]
    // Files containing NUL bytes are classified as binary.
    fn is_text_classifies_binary() {
        assert!(is_text(b"hello\nworld"));
        assert!(!is_text(b"\x00binary"));
    }
}
