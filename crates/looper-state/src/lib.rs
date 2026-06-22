//! `looper-state` — atomic, versioned JSON persistence for Looper.
//!
//! [`Store<T>`] loads/saves a document of a [`Versioned`] type as a
//! `{ "schema_version": N, "data": {…} }` envelope. Writes are atomic (temp file in
//! the same directory → fsync → rename), reads return [`Default`] when the file is
//! absent and apply forward [`Versioned::migrate`] steps when the on-disk version is
//! older. Files are pretty-printed, open-format JSON (a requirement from `mvp.md`).
//!
//! Dependency rule: **leaf** crate — the caller supplies the path (typically derived
//! from `looper_config::Dirs`), so this crate does not depend on `looper-config`.
//! See `../../AGENTS.md`. Implements plan item 06
//! (`../../specs/plan/06-state-json-persistence.md`).
//!
//! Out of scope for Milestone 1 (future work): cross-process file locking and a
//! SQLite-backed store behind the same API.

mod error;

use std::cmp::Ordering;
use std::io::Write;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub use error::StateError;

/// A document persisted as versioned JSON, with optional forward migrations.
pub trait Versioned: Serialize + DeserializeOwned + Default {
    /// The current on-disk schema version. Start at `1`; bump on a breaking change.
    const SCHEMA_VERSION: u32;

    /// Migrate a raw JSON `value` from `from_version` to `from_version + 1`.
    ///
    /// The default rejects any migration; override it once a breaking schema change
    /// ships. Called repeatedly by [`Store::load`] until the value reaches the current
    /// version.
    ///
    /// # Errors
    /// Returns [`StateError::UnsupportedVersion`] for versions it does not handle.
    fn migrate(
        from_version: u32,
        _value: serde_json::Value,
    ) -> Result<serde_json::Value, StateError> {
        Err(StateError::UnsupportedVersion {
            found: from_version,
            current: Self::SCHEMA_VERSION,
        })
    }
}

/// An atomic, versioned JSON document store backed by a single file.
#[derive(Debug, Clone)]
pub struct Store<T> {
    path: PathBuf,
    _marker: PhantomData<fn() -> T>,
}

impl<T: Versioned> Store<T> {
    /// Create a store for the document at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            _marker: PhantomData,
        }
    }

    /// The file path backing this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the backing file currently exists.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Load the document.
    ///
    /// Returns [`Default`] when the file does not exist, and applies forward
    /// migrations when the on-disk version is older than [`Versioned::SCHEMA_VERSION`].
    ///
    /// # Errors
    /// Returns [`StateError`] on I/O failure, parse failure, an unsupported migration,
    /// or a version newer than this build supports.
    pub fn load(&self) -> Result<T, StateError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
            Err(source) => {
                return Err(StateError::Read {
                    path: self.path.clone(),
                    source,
                })
            }
        };

        let envelope: Envelope =
            serde_json::from_slice(&bytes).map_err(|source| StateError::Parse {
                path: self.path.clone(),
                source,
            })?;

        let data = migrate_to_current::<T>(envelope)?;

        serde_json::from_value(data).map_err(|source| StateError::Parse {
            path: self.path.clone(),
            source,
        })
    }

    /// Atomically persist the document (write-temp + fsync + rename).
    ///
    /// # Errors
    /// Returns [`StateError`] on serialization or I/O failure.
    pub fn save(&self, value: &T) -> Result<(), StateError> {
        let data = serde_json::to_value(value).map_err(StateError::Serialize)?;
        let envelope = Envelope {
            schema_version: T::SCHEMA_VERSION,
            data,
        };
        atomic_write_json(&self.path, &envelope)
    }
}

/// On-disk wrapper around a document's data.
#[derive(Serialize, Deserialize)]
struct Envelope {
    schema_version: u32,
    data: serde_json::Value,
}

/// Apply forward migrations until `envelope.data` is at `T::SCHEMA_VERSION`.
fn migrate_to_current<T: Versioned>(envelope: Envelope) -> Result<serde_json::Value, StateError> {
    match envelope.schema_version.cmp(&T::SCHEMA_VERSION) {
        Ordering::Equal => Ok(envelope.data),
        Ordering::Greater => Err(StateError::FutureVersion {
            found: envelope.schema_version,
            current: T::SCHEMA_VERSION,
        }),
        Ordering::Less => {
            let mut version = envelope.schema_version;
            let mut data = envelope.data;
            while version < T::SCHEMA_VERSION {
                data = T::migrate(version, data)?;
                version += 1;
            }
            Ok(data)
        }
    }
}

/// Serialize `value` as pretty JSON and write it atomically to `path`.
fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StateError> {
    let dir = path.parent().ok_or_else(|| StateError::InvalidPath {
        path: path.to_path_buf(),
    })?;
    std::fs::create_dir_all(dir).map_err(|source| StateError::Write {
        path: dir.to_path_buf(),
        source,
    })?;

    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|source| StateError::Write {
        path: dir.to_path_buf(),
        source,
    })?;
    serde_json::to_writer_pretty(&mut tmp, value).map_err(StateError::Serialize)?;
    tmp.flush().map_err(|source| StateError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    tmp.as_file()
        .sync_all()
        .map_err(|source| StateError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    tmp.persist(path).map_err(|err| StateError::Persist {
        path: path.to_path_buf(),
        source: err.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct Doc {
        name: String,
        count: u32,
    }
    impl Versioned for Doc {
        const SCHEMA_VERSION: u32 = 1;
    }

    // A v2 type whose migrate() adds a field, to exercise the migration path.
    #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct DocV2 {
        name: String,
        count: u32,
        enabled: bool,
    }
    impl Versioned for DocV2 {
        const SCHEMA_VERSION: u32 = 2;
        fn migrate(
            from: u32,
            mut value: serde_json::Value,
        ) -> Result<serde_json::Value, StateError> {
            if from == 1 {
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("enabled".into(), serde_json::Value::Bool(true));
                }
                Ok(value)
            } else {
                Err(StateError::UnsupportedVersion {
                    found: from,
                    current: Self::SCHEMA_VERSION,
                })
            }
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("looper-state-{}-{name}", std::process::id()))
            .join("doc.json")
    }

    fn clean(path: &Path) {
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_missing_returns_default() {
        let store: Store<Doc> = Store::new(temp_path("missing"));
        clean(store.path());
        assert!(!store.exists());
        assert_eq!(store.load().unwrap(), Doc::default());
    }

    #[test]
    fn save_then_load_round_trips_and_is_readable() {
        let path = temp_path("roundtrip");
        clean(&path);
        let store: Store<Doc> = Store::new(&path);
        let doc = Doc {
            name: "looper".into(),
            count: 3,
        };
        store.save(&doc).unwrap();
        assert!(store.exists());
        assert_eq!(store.load().unwrap(), doc);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"schema_version\": 1"));
        assert!(text.contains("\"name\": \"looper\""));
        clean(&path);
    }

    #[test]
    fn migrates_old_version_on_load() {
        let path = temp_path("migrate");
        clean(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"schema_version":1,"data":{"name":"x","count":2}}"#,
        )
        .unwrap();

        let store: Store<DocV2> = Store::new(&path);
        assert_eq!(
            store.load().unwrap(),
            DocV2 {
                name: "x".into(),
                count: 2,
                enabled: true,
            }
        );
        clean(&path);
    }

    #[test]
    fn rejects_future_version() {
        let path = temp_path("future");
        clean(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"schema_version":99,"data":{"name":"x","count":0}}"#,
        )
        .unwrap();

        let store: Store<Doc> = Store::new(&path);
        assert!(matches!(
            store.load(),
            Err(StateError::FutureVersion {
                found: 99,
                current: 1
            })
        ));
        clean(&path);
    }

    #[test]
    fn corrupt_file_is_a_parse_error() {
        let path = temp_path("corrupt");
        clean(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json at all").unwrap();

        let store: Store<Doc> = Store::new(&path);
        assert!(matches!(store.load(), Err(StateError::Parse { .. })));
        clean(&path);
    }
}
