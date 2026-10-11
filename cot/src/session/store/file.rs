//! File session store
//!
//! This module provides a session store that uses the file system to store
//! session records.
//!
//! # Examples
//!
//! ```
//! use std::path::PathBuf;
//!
//! use cot::session::store::file::FileStore;
//!
//! let store = FileStore::new(PathBuf::from("/var/lib/cot/sessions"));
//! ```
use std::borrow::Cow;
use std::error::Error;
use std::io;
use std::path::Path;

use async_trait::async_trait;
use thiserror::Error;
use tokio::fs::{OpenOptions, remove_file};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower_sessions::session::{Id, Record};
use tower_sessions::{SessionStore, session_store};

use crate::session::store::{ERROR_PREFIX, MAX_COLLISION_RETRIES};

/// Errors that can occur when using the File session store.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FileStoreError {
    /// An error occurred during an I/O operation.
    #[error("{ERROR_PREFIX} I/O error: {0}")]
    Io(#[from] Box<dyn Error + Send + Sync>),
    /// The record ID collided too many times while saving in the database.
    #[error("{ERROR_PREFIX} session‐id collision retried too many times ({0})")]
    TooManyIdCollisions(u32),
    /// An error occurred during JSON serialization.
    #[error("{ERROR_PREFIX} JSON serialization error: {0}")]
    Serialize(Box<dyn Error + Send + Sync>),
    /// An error occurred during JSON deserialization.
    #[error("{ERROR_PREFIX} JSON serialization error: {0}")]
    Deserialize(Box<dyn Error + Send + Sync>),
}

impl From<FileStoreError> for session_store::Error {
    fn from(error: FileStoreError) -> session_store::Error {
        match error {
            FileStoreError::Io(inner) => session_store::Error::Backend(inner.to_string()),
            FileStoreError::Serialize(inner) => session_store::Error::Encode(inner.to_string()),
            FileStoreError::Deserialize(inner) => session_store::Error::Decode(inner.to_string()),
            other => session_store::Error::Backend(other.to_string()),
        }
    }
}

/// A file-based session store implementation.
///
/// This store persists sessions in a directory on the file system, providing
/// a simple and lightweight session storage solution.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use cot::session::store::file::FileStore;
///
/// let store = FileStore::new(PathBuf::from("/var/lib/cot/sessions"));
/// ```
#[derive(Debug, Clone)]
pub struct FileStore {
    /// The directory to save session files.
    dir_path: Cow<'static, Path>,
}

impl FileStore {
    /// Creates a new `FileStore` pointing at the given directory.
    ///
    /// # Errors
    ///
    /// Returns [`FileStoreError::Io`] if it fails to create the directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::borrow::Cow;
    /// use std::path::Path;
    ///
    /// use cot::session::store::file::FileStore;
    ///
    /// let store = FileStore::new(Cow::Borrowed(Path::new("/tmp/sessions")))
    ///     .expect("failed to create file store");
    /// ```
    pub fn new(dir_path: impl Into<Cow<'static, Path>>) -> Result<Self, FileStoreError> {
        let dir_path = dir_path.into();
        std::fs::create_dir_all(&dir_path).map_err(|err| FileStoreError::Io(Box::new(err)))?;

        let file_store = Self { dir_path };
        Ok(file_store)
    }

    async fn create_dir_if_not_exists(&self) -> Result<(), FileStoreError> {
        tokio::fs::create_dir_all(&self.dir_path)
            .await
            .map_err(|err| FileStoreError::Io(Box::new(err)))
    }
}

#[async_trait]
impl SessionStore for FileStore {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        for _ in 0..=MAX_COLLISION_RETRIES {
            let file_path = self.dir_path.join(record.id.to_string());
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&file_path)
                .await;

            match file {
                Ok(mut file) => {
                    let json_data = serde_json::to_string(&record)
                        .map_err(|err| FileStoreError::Serialize(Box::new(err)))?;
                    file.write_all(json_data.as_bytes())
                        .await
                        .map_err(|err| FileStoreError::Io(Box::new(err)))?;
                    // Await completion of Tokio's queued blocking write.
                    file.flush()
                        .await
                        .map_err(|err| FileStoreError::Io(Box::new(err)))?;
                    return Ok(());
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    // On collision, recycle the ID and try again.
                    record.id = Id::default();
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    self.create_dir_if_not_exists().await?;
                }
                Err(err) => return Err(FileStoreError::Io(Box::new(err)))?,
            }
        }

        Err(FileStoreError::TooManyIdCollisions(MAX_COLLISION_RETRIES))?
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(self.dir_path.join(record.id.to_string()))
            .await;

        match file {
            Ok(mut file) => {
                let json_data = serde_json::to_string(&record)
                    .map_err(|err| FileStoreError::Serialize(Box::new(err)))?;
                file.write_all(json_data.as_bytes())
                    .await
                    .map_err(|err| FileStoreError::Io(Box::new(err)))?;
                file.flush()
                    .await
                    .map_err(|err| FileStoreError::Io(Box::new(err)))?;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // create the file if it does not exist.
                let mut record = record.clone();
                self.create(&mut record).await?;
            }
            Err(err) => Err(FileStoreError::Io(Box::new(err)))?,
        }

        Ok(())
    }

    async fn load(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        let path = self.dir_path.join(session_id.to_string());
        if !path.is_file() {
            return Ok(None);
        }
        let mut file = OpenOptions::new()
            .read(true)
            .open(path)
            .await
            .map_err(|err| FileStoreError::Io(Box::new(err)))?;

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .map_err(|err| FileStoreError::Io(Box::new(err)))?;
        let out = serde_json::from_str(&contents)
            .map_err(|err| FileStoreError::Serialize(Box::new(err)))?;

        Ok(out)
    }

    async fn delete(&self, session_id: &Id) -> session_store::Result<()> {
        let res = remove_file(self.dir_path.join(session_id.to_string())).await;
        if let Err(e) = res
            && e.kind() != io::ErrorKind::NotFound
        {
            Err(FileStoreError::Io(Box::new(e)))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tempfile::tempdir;
    use time::{Duration, OffsetDateTime};
    use tokio::fs;
    use tower_sessions::session::{Id, Record};

    use super::*;

    fn make_store() -> FileStore {
        let dir = tempdir().expect("failed to make tempdir");
        FileStore::new(dir.keep()).expect("could not create file store")
    }

    fn make_record() -> Record {
        Record {
            id: Id::default(),
            data: HashMap::default(),
            expiry_date: OffsetDateTime::now_utc() + Duration::minutes(30),
        }
    }

    #[cot::test]
    async fn test_create_and_load() {
        let store = make_store();
        let mut rec = make_record();
        store.create(&mut rec).await.expect("create failed");
        let path = store.dir_path.join(rec.id.to_string());
        assert!(path.is_file(), "session file wasn't created");

        let loaded = store.load(&rec.id).await.unwrap();
        assert_eq!(Some(rec.clone()), loaded);
    }

    #[cot::test]
    async fn test_save_overwrites() {
        let store = make_store();
        let mut rec = make_record();
        store.create(&mut rec).await.unwrap();

        let mut rec2 = rec.clone();
        rec2.data.insert("foo".into(), "bar".into());
        store.save(&rec2).await.expect("save failed");

        let loaded = store.load(&rec.id).await.unwrap().unwrap();
        assert_eq!(rec2.data, loaded.data);
    }

    #[cot::test]
    async fn test_save_creates_if_missing() {
        let store = make_store();
        let rec = make_record();
        store.save(&rec).await.unwrap();

        let path = store.dir_path.join(rec.id.to_string());
        assert!(path.is_file());
    }

    #[cot::test]
    async fn test_save_creates_directory() {
        let dir = tempdir().expect("failed to make tempdir");
        let dir_path = dir.path().to_path_buf();
        // we only want a valid and safe disposable path.
        dir.close().expect("failed to remove tempdir");
        assert!(!dir_path.exists());

        let store = FileStore::new(dir_path.clone()).expect("could not create file store");
        let rec = make_record();
        store
            .save(&rec)
            .await
            .expect("save should succeed and create directory");
        assert!(dir_path.exists(), "Directory should be created when saving");

        // Now manually delete the directory
        fs::remove_dir_all(&dir_path)
            .await
            .expect("failed to remove directory");
        assert!(!dir_path.exists(), "Directory should be removed");

        // Saving again should recreate the directory
        store
            .save(&rec)
            .await
            .expect("save should recreate directory");
        assert!(
            dir_path.exists(),
            "Directory should be recreated when saving"
        );

        fs::remove_dir_all(&dir_path).await.expect("cleanup failed");
    }

    #[cot::test]
    async fn test_load_with_nonexistent_directory() {
        let dir = tempdir().expect("failed to make tempdir");
        let dir_path = dir.path().to_path_buf();

        let store = FileStore::new(dir_path.clone()).expect("could not create file store");
        dir.close().expect("failed to remove tempdir");

        let id = Id::default();
        let result = store.load(&id).await;
        assert!(
            result.is_ok(),
            "Load should not error with non-existent directory"
        );
        assert!(
            result.unwrap().is_none(),
            "Load should return None with non-existent directory"
        );

        assert!(
            !dir_path.exists(),
            "Directory should not be created when just loading"
        );
    }

    #[cot::test]
    async fn test_delete() {
        let store = make_store();
        let mut rec = make_record();
        store.create(&mut rec).await.unwrap();

        store.delete(&rec.id).await.unwrap();
        let path = store.dir_path.join(rec.id.to_string());
        assert!(!path.exists());

        store.delete(&rec.id).await.unwrap();
    }

    #[cot::test]
    async fn test_delete_with_nonexistent_directory() {
        let dir = tempdir().expect("failed to make tempdir");
        let dir_path = dir.path().to_path_buf();
        let store = FileStore::new(dir_path.clone()).expect("could not create file store");
        dir.close().expect("failed to remove tempdir");

        // Delete should work with non-existent directory
        let id = Id::default();
        let result = store.delete(&id).await;
        assert!(
            result.is_ok(),
            "Delete should not error with non-existent directory"
        );

        assert!(
            !dir_path.exists(),
            "Directory should not be created when just deleting"
        );
    }

    #[cot::test]
    async fn test_create_id_collision() {
        let store = make_store();
        let expiry = OffsetDateTime::now_utc() + Duration::minutes(30);

        let mut r1 = Record {
            id: Id::default(),
            data: HashMap::default(),
            expiry_date: expiry,
        };
        store.create(&mut r1).await.unwrap();

        let mut r2 = Record {
            id: r1.id,
            data: HashMap::default(),
            expiry_date: expiry,
        };
        store.create(&mut r2).await.unwrap();

        assert_ne!(r1.id, r2.id, "ID collision not resolved");
        let p1 = store.dir_path.join(r1.id.to_string());
        let p2 = store.dir_path.join(r2.id.to_string());
        assert!(p1.is_file() && p2.is_file());
    }

    #[cot::test]
    async fn test_from_file_store_error_to_session_store_error() {
        let io_err = io::Error::other("io problem");
        let sess_err: session_store::Error = FileStoreError::Io(Box::new(io_err)).into();
        assert!(matches!(sess_err, session_store::Error::Backend(_)));

        let ser_err = io::Error::other("serialize fail");
        let sess_err: session_store::Error = FileStoreError::Serialize(Box::new(ser_err)).into();
        assert!(matches!(sess_err, session_store::Error::Encode(_)));

        let parse_err = serde_json::from_str::<Record>("not a json").unwrap_err();
        let sess_err: session_store::Error =
            FileStoreError::Deserialize(Box::new(parse_err)).into();
        assert!(matches!(sess_err, session_store::Error::Decode(_)));

        let sess_err: session_store::Error = FileStoreError::TooManyIdCollisions(42).into();
        assert!(matches!(sess_err, session_store::Error::Backend(_)));
    }

    struct BlockingGate {
        release: Option<std::sync::mpsc::Sender<()>>,
        started: std::sync::mpsc::Receiver<()>,
        task: tokio::task::JoinHandle<()>,
    }

    impl BlockingGate {
        fn spawn() -> Self {
            let (release, wait) = std::sync::mpsc::channel();
            let (acknowledge, started) = std::sync::mpsc::channel();
            let task = tokio::task::spawn_blocking(move || {
                acknowledge.send(()).expect("gate receiver is alive");
                let _ = wait.recv_timeout(std::time::Duration::from_secs(30));
            });
            Self {
                release: Some(release),
                started,
                task,
            }
        }

        fn wait_until_started(&self) {
            self.started
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("blocking gate should start");
        }

        fn release(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
        }
    }

    impl Drop for BlockingGate {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn assert_write_waits_for_completion(create: bool) {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll, Waker};

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempdir().unwrap();
            let store = FileStore::new(dir.path().to_path_buf()).unwrap();
            let mut record = make_record();
            record.data.insert("revision".into(), "new".into());
            let expected = serde_json::to_vec(&record).unwrap();
            let id = record.id;
            let path = dir.path().join(id.to_string());
            if create {
                assert!(!path.exists());
            } else {
                let mut previous = record.clone();
                previous.data.insert("revision".into(), "old".into());
                let previous_bytes = serde_json::to_vec(&previous).unwrap();
                std::fs::write(&path, &previous_bytes).unwrap();
                assert_eq!(std::fs::read(&path).unwrap(), previous_bytes);
            }

            let mut opening_gate = BlockingGate::spawn();
            opening_gate.wait_until_started();
            let mut operation: Pin<Box<dyn Future<Output = session_store::Result<()>> + Send>> =
                if create {
                    store.create(&mut record)
                } else {
                    store.save(&record)
                };
            let mut context = Context::from_waker(Waker::noop());
            assert!(operation.as_mut().poll(&mut context).is_pending());

            // Queue a second gate after open, but before polling its result.
            // With one blocking worker this prevents the subsequent write.
            let mut writing_gate = BlockingGate::spawn();
            opening_gate.release();
            writing_gate.wait_until_started();
            assert!(path.is_file(), "open must have completed before the gate");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

            let awaiting_write = match operation.as_mut().poll(&mut context) {
                Poll::Pending => true,
                Poll::Ready(Ok(())) => false,
                Poll::Ready(Err(error)) => panic!("unexpected storage error: {error}"),
            };
            assert!(std::fs::read(&path).unwrap().is_empty());
            assert!(!writing_gate.task.is_finished());
            writing_gate.release();
            if awaiting_write {
                tokio::time::timeout(std::time::Duration::from_secs(10), operation)
                    .await
                    .expect("write should finish after releasing the gate")
                    .unwrap();
            } else {
                drop(operation);
            }
            // On the unfixed implementation, this queued load also drains the
            // earlier write. Check the completion contract separately below.
            let loaded = tokio::time::timeout(std::time::Duration::from_secs(10), store.load(&id))
                .await
                .expect("load should finish")
                .unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), expected);
            assert_eq!(loaded, Some(record));
            assert!(awaiting_write, "must wait for the blocking write to finish");
        });
    }

    #[test]
    fn test_create_waits_for_write_completion() {
        assert_write_waits_for_completion(true);
    }

    #[test]
    fn test_save_waits_for_write_completion() {
        assert_write_waits_for_completion(false);
    }

    #[cfg(target_os = "linux")]
    #[cot::test]
    async fn test_save_propagates_delayed_write_error() {
        use std::os::unix::fs::FileTypeExt;

        assert!(
            std::fs::metadata("/dev/full")
                .unwrap()
                .file_type()
                .is_char_device()
        );
        let dir = tempdir().unwrap();
        let store = FileStore::new(dir.path().to_path_buf()).unwrap();
        let record = make_record();
        let path = dir.path().join(record.id.to_string());
        std::os::unix::fs::symlink("/dev/full", &path).unwrap();
        // Opening succeeds; the device rejects the later write. Tokio may
        // report that error only when its buffered write is flushed.
        let opened = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .await
            .expect("the failing-write device should open successfully");
        drop(opened);
        let error = store.save(&record).await.unwrap_err();
        assert!(matches!(error, session_store::Error::Backend(_)));
    }
}
