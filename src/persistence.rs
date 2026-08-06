use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;

const FORMAT_VERSION: u32 = 2;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PersistedSession {
    pub workspace: PathBuf,
    pub current_tab: Option<String>,
    pub last_activity_unix_seconds: u64,
    pub cleanup_pending: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoredState {
    pub owners: HashMap<String, String>,
    pub sessions: HashMap<String, PersistedSession>,
    pub owner_revision: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct StoredState {
    version: u32,
    generation: String,
    owners: HashMap<String, String>,
    sessions: HashMap<String, PersistedSession>,
    owner_revision: u64,
}

pub struct OwnershipPersistence {
    path: PathBuf,
    state: Mutex<StoredState>,
}

impl OwnershipPersistence {
    pub fn open(path: PathBuf) -> Result<Arc<Self>> {
        let state = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.permissions().mode() & 0o077 != 0
                    || metadata.len() > MAX_STATE_BYTES
                {
                    bail!("unsafe browser ownership state file: {}", path.display());
                }
                let bytes = fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let state: StoredState = serde_json::from_slice(&bytes).with_context(|| {
                    format!("invalid browser ownership state {}", path.display())
                })?;
                if state.version != FORMAT_VERSION {
                    warn!(
                        actual = state.version,
                        expected = FORMAT_VERSION,
                        "discarding unsupported browser ownership state"
                    );
                    StoredState::default()
                } else {
                    state
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StoredState::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
            }
        };
        Ok(Arc::new(Self {
            path,
            state: Mutex::new(state),
        }))
    }

    pub fn activate_generation(&self, generation: &str) -> Result<RestoredState> {
        self.commit(|state| {
            if state.version != FORMAT_VERSION || state.generation != generation {
                *state = StoredState {
                    version: FORMAT_VERSION,
                    generation: generation.to_owned(),
                    ..StoredState::default()
                };
            }
            state
                .owners
                .retain(|_, session| state.sessions.contains_key(session));
            state
                .sessions
                .retain(|session, _| state.owners.values().any(|owner| owner == session));
            Ok(RestoredState {
                owners: state.owners.clone(),
                sessions: state.sessions.clone(),
                owner_revision: state.owner_revision,
            })
        })
    }

    pub fn generation(&self) -> Option<String> {
        let state = self.state.lock().expect("ownership persistence poisoned");
        (state.version == FORMAT_VERSION && !state.generation.is_empty())
            .then(|| state.generation.clone())
    }

    pub fn clear_generation(&self, generation: &str) -> Result<()> {
        self.commit(|state| {
            if state.version != FORMAT_VERSION || state.generation != generation {
                bail!("browser generation changed before terminal cleanup");
            }
            state.owners.clear();
            state.sessions.clear();
            state.owner_revision = 0;
            Ok(())
        })
    }

    pub fn replace_owners(
        &self,
        generation: &str,
        revision: u64,
        owners: HashMap<String, String>,
    ) -> Result<()> {
        self.commit(|state| {
            if state.version != FORMAT_VERSION || state.generation != generation {
                bail!("browser ownership generation changed while persisting state");
            }
            if revision < state.owner_revision {
                return Ok(());
            }
            if revision == state.owner_revision {
                if state.owners != owners {
                    bail!("conflicting browser ownership snapshots have the same revision");
                }
                return Ok(());
            }
            state.owners = owners;
            state.owner_revision = revision;
            Ok(())
        })
    }

    pub fn update_session(&self, id: &str, mut session: PersistedSession) -> Result<()> {
        self.commit(|state| {
            if state
                .sessions
                .get(id)
                .is_some_and(|current| current.cleanup_pending)
            {
                session.cleanup_pending = true;
            }
            state.sessions.insert(id.to_owned(), session);
            Ok(())
        })
    }

    pub fn request_cleanup(&self, id: &str) -> Result<bool> {
        self.commit(|state| {
            let Some(session) = state.sessions.get_mut(id) else {
                return Ok(false);
            };
            session.cleanup_pending = true;
            Ok(true)
        })
    }

    pub fn remove_session_if_unowned(&self, id: &str) -> Result<()> {
        self.commit(|state| {
            if !state.owners.values().any(|owner| owner == id) {
                state.sessions.remove(id);
            }
            Ok(())
        })
    }

    /// Applies one state transition after atomically replacing the state file.
    fn commit<T>(&self, update: impl FnOnce(&mut StoredState) -> Result<T>) -> Result<T> {
        let mut current = self.state.lock().expect("ownership persistence poisoned");
        let mut next = current.clone();
        let result = update(&mut next)?;
        if next != *current {
            self.replace(&mut current, next)?;
        }
        Ok(result)
    }

    fn replace(&self, current: &mut StoredState, next: StoredState) -> Result<()> {
        let bytes =
            serde_json::to_vec(&next).context("failed to encode browser ownership state")?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            bail!("browser ownership state exceeds {MAX_STATE_BYTES} bytes");
        }
        let parent = self
            .path
            .parent()
            .context("browser ownership state path has no parent")?;
        let temporary = loop {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".state.json.{}.{}.tmp",
                std::process::id(),
                sequence
            ));
            if !candidate.exists() {
                break candidate;
            }
        };
        let installed = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)
                .with_context(|| format!("failed to create {}", temporary.display()))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path).with_context(|| {
                format!(
                    "failed to replace browser ownership state {}",
                    self.path.display()
                )
            })?;
            Ok(())
        })();
        if installed.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        installed?;
        // The rename is already visible. Keep memory aligned with it even if
        // synchronizing the directory reports a durability error.
        *current = next;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "opencode-browser-state-{}-{}-{name}.json",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn restores_only_matching_generation_and_known_sessions() {
        let path = state_path("restore");
        let store = OwnershipPersistence::open(path.clone()).unwrap();
        store.activate_generation("generation-a").unwrap();
        store
            .update_session(
                "session-a",
                PersistedSession {
                    workspace: PathBuf::from("/workspace"),
                    current_tab: Some("tab-a".into()),
                    last_activity_unix_seconds: 10,
                    cleanup_pending: true,
                },
            )
            .unwrap();
        store
            .update_session(
                "orphan-session",
                PersistedSession {
                    workspace: PathBuf::from("/workspace"),
                    current_tab: None,
                    last_activity_unix_seconds: 10,
                    cleanup_pending: false,
                },
            )
            .unwrap();
        store
            .replace_owners(
                "generation-a",
                1,
                HashMap::from([
                    ("tab-a".into(), "session-a".into()),
                    ("orphan-tab".into(), "missing-session".into()),
                ]),
            )
            .unwrap();
        drop(store);

        let store = OwnershipPersistence::open(path.clone()).unwrap();
        let restored = store.activate_generation("generation-a").unwrap();
        assert_eq!(
            restored.owners.get("tab-a").map(String::as_str),
            Some("session-a")
        );
        assert!(restored.sessions.contains_key("session-a"));
        assert!(restored.sessions["session-a"].cleanup_pending);
        assert_eq!(restored.owners.len(), 1);
        assert_eq!(restored.sessions.len(), 1);
        let reset = store.activate_generation("generation-b").unwrap();
        assert!(reset.owners.is_empty());
        assert!(reset.sessions.is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn stale_owner_snapshot_cannot_overwrite_newer_revision() {
        let path = state_path("revision");
        let store = OwnershipPersistence::open(path.clone()).unwrap();
        store.activate_generation("generation").unwrap();
        for id in ["session-a", "session-b"] {
            store
                .update_session(
                    id,
                    PersistedSession {
                        workspace: PathBuf::from("/workspace"),
                        current_tab: None,
                        last_activity_unix_seconds: 1,
                        cleanup_pending: false,
                    },
                )
                .unwrap();
        }
        let current = HashMap::from([
            ("tab-a".into(), "session-a".into()),
            ("tab-b".into(), "session-b".into()),
        ]);
        store
            .replace_owners("generation", 2, current.clone())
            .unwrap();
        store
            .replace_owners(
                "generation",
                1,
                HashMap::from([("tab-a".into(), "session-a".into())]),
            )
            .unwrap();
        drop(store);
        let restored = OwnershipPersistence::open(path.clone())
            .unwrap()
            .activate_generation("generation")
            .unwrap();
        assert_eq!(restored.owners, current);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn cleanup_requests_are_durable_and_monotonic() {
        let path = state_path("cleanup");
        let store = OwnershipPersistence::open(path.clone()).unwrap();
        store.activate_generation("generation").unwrap();
        let session = PersistedSession {
            workspace: PathBuf::from("/workspace"),
            current_tab: Some("tab-a".into()),
            last_activity_unix_seconds: 1,
            cleanup_pending: false,
        };
        store.update_session("session-a", session.clone()).unwrap();
        assert!(store.request_cleanup("session-a").unwrap());
        assert!(!store.request_cleanup("missing").unwrap());
        store.update_session("session-a", session).unwrap();
        store
            .replace_owners(
                "generation",
                1,
                HashMap::from([("tab-a".into(), "session-a".into())]),
            )
            .unwrap();
        drop(store);

        let restored = OwnershipPersistence::open(path.clone())
            .unwrap()
            .activate_generation("generation")
            .unwrap();
        assert!(restored.sessions["session-a"].cleanup_pending);
        let _ = fs::remove_file(path);
    }
}
