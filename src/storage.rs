// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

// This class stores uploaded files and associated metadata.
// Files are immutable but can be deleted.
//
// A unique file is defined by a `scope` and a `filename`.
// The filename can be any valid UTF-8 string up to 1KB
// in size and does not have to be a valid filesystem name.
//
// The list of files and their metadata are stored in a single
// SQLite database table. Small files are stored in a BLOB column
// in SQLite; for larger files, a checksum is stored in the database,
// and the file itself is stored on disk.
//
// On disk new files are written in two steps:
// - the file is written to a temporary file in the same directory;
// - a transaction is opened and a new file entry is created
// - temp file is renamed to the final filename
// - the transaction is committed.
//
// This scheme guarantees that if a record exists in the table, the file is written.
// However if the server crashes after rename but before commit, an orphan file will remain on the disk.
// To combat this, when the server starts, we check that there is an entry
// in the database for each file on the disk, and we also delete all temporary (non-renamed) files.

use std::path::{Path, PathBuf};
use std::sync;

use bytes::Bytes;
use crc32fast::Hasher as Crc32Hasher;
use sqlx::encode::{Encode, IsNull};
use sqlx::error::BoxDynError;
use sqlx::{FromRow, Pool, Sqlite, query, query_as, query_scalar, sqlite};
use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, instrument, warn};
use uuid::Uuid;

use super::backends::{BackendError, BackendStorage, RawRelease, RawReleaseFile, Release};
use super::config::ConfigService;

const SQLITE_POOL_SIZE: u32 = 16;
const INLINE_THRESHOLD: usize = 256 * 1024; // 256 KB

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Id(pub Uuid);

impl std::ops::Deref for Id {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl sqlx::Type<Sqlite> for Id {
    fn type_info() -> sqlite::SqliteTypeInfo {
        <Vec<u8> as sqlx::Type<Sqlite>>::type_info()
    }

    fn compatible(ty: &sqlite::SqliteTypeInfo) -> bool {
        <Vec<u8> as sqlx::Type<Sqlite>>::compatible(ty)
    }
}

impl<'r> sqlx::decode::Decode<'r, Sqlite> for Id {
    fn decode(
        value: sqlite::SqliteValueRef<'r>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let value: Vec<u8> = <Vec<u8> as sqlx::decode::Decode<Sqlite>>::decode(value)?;
        let uuid = Uuid::from_slice(&value)?;
        Ok(Id(uuid))
    }
}

impl<'q> Encode<'q, Sqlite> for Id {
    fn encode_by_ref(
        &self,
        buf: &mut <Sqlite as sqlx::database::Database>::ArgumentBuffer<'q>,
    ) -> Result<IsNull, BoxDynError> {
        let value = self.0.as_bytes().to_vec();
        <Vec<u8> as Encode<Sqlite>>::encode(value, buf)
    }
}

#[derive(Debug, Clone)]
pub struct Blob(pub Bytes);

impl std::ops::Deref for Blob {
    type Target = Bytes;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl sqlx::Type<Sqlite> for Blob {
    fn type_info() -> sqlite::SqliteTypeInfo {
        <Vec<u8> as sqlx::Type<Sqlite>>::type_info() // BLOB
    }

    fn compatible(ty: &sqlite::SqliteTypeInfo) -> bool {
        <Vec<u8> as sqlx::Type<Sqlite>>::compatible(ty)
    }
}

impl<'r> sqlx::decode::Decode<'r, Sqlite> for Blob {
    fn decode(
        value: sqlite::SqliteValueRef<'r>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let slice: &'r [u8] = <&'r [u8] as sqlx::decode::Decode<Sqlite>>::decode(value)?;
        Ok(Blob(Bytes::copy_from_slice(slice)))
    }
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("file already exists: {0}/{1}")]
    AlreadyExists(String, String),

    #[error("blob integrity check failed")]
    IntegrityError,

    #[error("blob file missing on disk: {0}")]
    BlobNotFound(PathBuf),

    #[error("failed to connect index db: {0}")]
    DbError(#[from] sqlx::Error),

    #[error("io error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Cached file entry (datafiles table)
#[allow(unused)]
#[derive(Debug, Clone, FromRow)]
pub struct Object {
    pub id: Id,
    pub scope: String,
    pub created_at: chrono::DateTime<chrono::Utc>,

    pub file_name: String,
    pub file_size: i64,
    pub file_bytes: Blob,
    pub inlined: bool,
}

impl Object {
    // You cannot change this ID, this will desynchronize the records in the database and the files on the disk.
    const UUID_ROOT_NAMESPACE: Uuid = Uuid::from_bytes([
        0x8b, 0x06, 0x3c, 0x4c, 0x6b, 0x5c, 0x4a, 0x8b, 0x92, 0x8f, 0x75, 0x8b, 0x0e, 0x63, 0xc3,
        0x5d,
    ]);

    fn uuid(scope: &str, file: &str) -> Id {
        let scope_ns = Uuid::new_v5(&Object::UUID_ROOT_NAMESPACE, scope.as_bytes());
        Id(Uuid::new_v5(&scope_ns, file.as_bytes()))
    }

    fn hash(scope: &str, file: &str, blob: &[u8]) -> Vec<u8> {
        let mut hasher = Crc32Hasher::new();

        hasher.update(b"/");
        hasher.update(scope.as_bytes());

        hasher.update(b"/");
        hasher.update(file.as_bytes());

        hasher.update(b"/");
        hasher.update(blob);
        hasher.update(b"\0");

        hasher.finalize().to_be_bytes().to_vec()
    }
}

/// Builder for file query SQL
pub struct FileQuery<'a> {
    backend: &'a str,
    version: Option<&'a str>,
    filename: Option<&'a str>,
    meta_null: Option<&'a str>,
}

impl<'a> FileQuery<'a> {
    pub fn new(backend: &'a str) -> Self {
        Self {
            backend,
            version: None,
            filename: None,
            meta_null: None,
        }
    }

    pub fn version(mut self, version: &'a str) -> Self {
        self.version = Some(version);
        self
    }

    pub fn filename(mut self, filename: &'a str) -> Self {
        self.filename = Some(filename);
        self
    }

    pub fn where_meta_null(mut self, field: &'a str) -> Self {
        self.meta_null = Some(field);
        self
    }

    pub fn build_sql(&self) -> String {
        let mut sql = String::from(
            "SELECT backend, version, filename, checksum, size, os, arch, meta FROM files WHERE backend = ?1",
        );
        let mut param_idx = 1;

        if self.version.is_some() {
            param_idx += 1;
            sql.push_str(&format!(" AND version = ?{}", param_idx));
        }
        if self.filename.is_some() {
            param_idx += 1;
            sql.push_str(&format!(" AND filename = ?{}", param_idx));
        }
        if let Some(field) = self.meta_null {
            sql.push_str(&format!(
                " AND (meta IS NULL OR json_extract(meta, '$.{}') IS NULL)",
                field
            ));
        }

        sql
    }
}

struct FileSystem {
    objects: PathBuf,
    database: PathBuf,
}

impl FileSystem {
    fn new(root: &Path) -> Self {
        Self {
            objects: root.join("objects"),
            database: root.join("index.sqlite"),
        }
    }

    fn database(&self) -> &Path {
        &self.database
    }

    fn objects_root(&self) -> &Path {
        &self.objects
    }

    fn object(&self, scope: &str, file: &str) -> PathBuf {
        let id = Object::uuid(scope, file);
        let hash_hex = hex::encode(id.0.as_bytes());
        self.objects_root()
            .join(&hash_hex[0..2])
            .join(&hash_hex[2..4])
            .join(&hash_hex)
    }

    async fn objects_walk<F, Fut>(&self, mut f: F) -> Result<(), StorageError>
    where
        F: FnMut(PathBuf) -> Fut,
        Fut: std::future::Future<Output = Result<(), StorageError>>,
    {
        if !self.objects.exists() {
            return Ok(());
        }

        let mut stack = vec![self.objects.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut entries = fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let file_type = entry.file_type().await?;

                if file_type.is_dir() {
                    stack.push(path);
                } else if file_type.is_file() {
                    f(path).await?;
                }
            }
        }

        Ok(())
    }
}

pub struct StorageService {
    blobfs: FileSystem,
    sqlite: Pool<Sqlite>,
}

impl StorageService {
    pub async fn new(config: sync::Arc<ConfigService>) -> Result<Self, StorageError> {
        let blobfs = FileSystem::new(config.dirname());

        let connection = format!("sqlite:{}?mode=rwc", blobfs.database().to_str().unwrap());
        let sqlite: Pool<Sqlite> = sqlite::SqlitePoolOptions::new()
            .max_connections(SQLITE_POOL_SIZE)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    // Connection-specific PRAGMAs (must be set on each connection)
                    sqlx::query("PRAGMA foreign_keys = ON;")
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("PRAGMA busy_timeout = 5000;")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&connection)
            .await?;

        // WAL mode is database-wide and persists, only needs to be set once
        query("PRAGMA journal_mode = WAL;").execute(&sqlite).await?;

        let storage = Self { sqlite, blobfs };
        storage.migrations().await?;
        storage.doctor().await?;

        Ok(storage)
    }

    /// Synchronously traverses the tree and removes temporary files.
    /// Must run before the application starts.
    async fn doctor(&self) -> Result<(), StorageError> {
        let result = self.blobfs.objects_walk(|path| async move {
            let name = match path.file_name().and_then(|name| name.to_str()) {
                Some(name) => name.to_string(),
                None => {
                    warn!(path = %path.display(), "cleanup: invalid blob name");
                    let _ = fs::remove_file(path).await;
                    return Ok(());
                }
            };

            if name.ends_with(".part") {
                warn!(path = %path.display(), "cleanup: removing temp file");
                let _ = fs::remove_file(path).await;
                return Ok(());
            }

            if name.len() != 32 {
                warn!(path = %path.display(), "cleanup: invalid blob name length");
                let _ = fs::remove_file(path).await;
                return Ok(());
            }

            let id_bytes = match hex::decode(&name) {
                Ok(bytes) => bytes,
                Err(_) => {
                    warn!(path = %path.display(), "cleanup: invalid blob name hex");
                    let _ = fs::remove_file(path).await;
                    return Ok(());
                }
            };

            let id = match Uuid::from_slice(&id_bytes) {
                Ok(uuid) => Id(uuid),
                Err(_) => {
                    warn!(path = %path.display(), "cleanup: invalid blob uuid");
                    let _ = fs::remove_file(path).await;
                    return Ok(());
                }
            };

            let exists: Option<i64> = query_scalar("SELECT 1 FROM datafiles WHERE id = ?1")
                .bind(id)
                .fetch_optional(&self.sqlite)
                .await?;

            if exists.is_none() {
                warn!(path = %path.display(), "cleanup: removing orphan blob");
                let _ = fs::remove_file(path).await;
            }

            Ok(())
        });
        result.await?;

        Ok(())
    }

    async fn migrations(&self) -> Result<(), StorageError> {
        // Object storage table
        query(
            "CREATE TABLE IF NOT EXISTS datafiles(
                id         BLOB    PRIMARY KEY CHECK (length(id) = 16),
                scope      TEXT    NOT NULL,
                created_at TEXT    DEFAULT (datetime('now')),
                file_name  TEXT    NOT NULL,
                file_size  INTEGER NOT NULL,
                file_bytes BLOB,
                inlined    INTEGER NOT NULL,
                UNIQUE (scope, file_name)
            ) STRICT",
        )
        .execute(&self.sqlite)
        .await?;

        // Drop old backend-specific tables (data will be re-fetched from upstream)
        query("DROP TABLE IF EXISTS go_files")
            .execute(&self.sqlite)
            .await?;
        query("DROP TABLE IF EXISTS go_versions")
            .execute(&self.sqlite)
            .await?;
        query("DROP TABLE IF EXISTS zig_files")
            .execute(&self.sqlite)
            .await?;
        query("DROP TABLE IF EXISTS zig_versions")
            .execute(&self.sqlite)
            .await?;

        // Unified versions table
        query(
            "CREATE TABLE IF NOT EXISTS versions (
                backend  TEXT    NOT NULL,
                version  TEXT    NOT NULL,
                sort_key INTEGER NOT NULL,
                meta     BLOB    CHECK (meta IS NULL OR (json_valid(meta) AND json_type(meta) = 'object')),
                PRIMARY KEY (backend, version)
            ) STRICT",
        )
        .execute(&self.sqlite)
        .await?;

        // Unified files table
        query(
            "CREATE TABLE IF NOT EXISTS files (
                backend   TEXT    NOT NULL,
                version   TEXT    NOT NULL,
                filename  TEXT    NOT NULL,
                checksum  TEXT    NOT NULL,
                size      INTEGER NOT NULL,
                os        TEXT,
                arch      TEXT,
                meta      BLOB    CHECK (meta IS NULL OR (json_valid(meta) AND json_type(meta) = 'object')),
                PRIMARY KEY (backend, version, filename),
                FOREIGN KEY (backend, version) REFERENCES versions(backend, version)
            ) STRICT",
        )
        .execute(&self.sqlite)
        .await?;

        // Index for fast filename lookups
        query("CREATE INDEX IF NOT EXISTS idx_files_backend_filename ON files(backend, filename)")
            .execute(&self.sqlite)
            .await?;

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn get(&self, scope: &str, filename: &str) -> Result<Option<Object>, StorageError> {
        let file: Option<Object> =
            query_as("SELECT * FROM datafiles WHERE scope = ?1 AND file_name = ?2")
                .bind(scope)
                .bind(filename)
                .fetch_optional(&self.sqlite)
                .await?;

        match file {
            None => {
                debug!("get: file not found");
                Ok(None)
            }
            Some(mut file) if !file.inlined => {
                let obj = self.blobfs.object(scope, filename);
                let bytes = match fs::read(&obj).await {
                    Ok(b) => Bytes::from(b),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Err(StorageError::BlobNotFound(obj));
                    }
                    Err(e) => return Err(e.into()),
                };

                let hash = Object::hash(scope, filename, &bytes);
                if hash != file.file_bytes.to_vec() {
                    return Err(StorageError::IntegrityError);
                }

                file.file_bytes = Blob(bytes);
                debug!(size = file.file_size, "get: loaded from disk");
                Ok(Some(file))
            }
            Some(file) => {
                debug!(size = file.file_size, "get: loaded inline");
                Ok(Some(file))
            }
        }
    }

    #[instrument(skip(self, bytes), fields(size = bytes.len()))]
    pub async fn put(
        &self,
        scope: &str,
        filename: &str,
        bytes: &Bytes,
    ) -> Result<(), StorageError> {
        let inlined = bytes.len() <= INLINE_THRESHOLD;

        let obj = self.blobfs.object(scope, filename);
        let tmp = obj.with_extension(format!("{}.part", Uuid::new_v4()));

        // write temp file
        if !inlined {
            if let Some(parent) = obj.parent() {
                fs::create_dir_all(parent).await?;
            }

            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .await?;

            file.write_all(bytes).await?;
            file.sync_all().await?;
        }

        // bytes for small files or hash for large ones
        let payload: Vec<u8> = if inlined {
            bytes.to_vec()
        } else {
            Object::hash(scope, filename, bytes)
        };

        let result = async {
            let mut tx = self.sqlite.begin().await?;
            let id = Object::uuid(scope, filename);

            let result = query(
                "
                INSERT INTO datafiles (
                    id, scope, file_name, file_size, file_bytes, inlined
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6
                );
                ",
            )
            .bind(id)
            .bind(scope)
            .bind(filename)
            .bind(bytes.len() as i64)
            .bind(&payload)
            .bind(inlined)
            .execute(tx.as_mut())
            .await;

            match result {
                Ok(_) => {
                    if !inlined {
                        fs::rename(&tmp, &obj).await?;
                        if let Some(parent) = obj.parent() {
                            let dir = fs::File::open(parent).await?;
                            dir.sync_all().await?;
                        }
                    }

                    tx.commit().await?;
                    debug!("put: a new file has been commited");
                    Ok(())
                }
                Err(sqlx::Error::Database(ref db_err)) if db_err.is_unique_violation() => {
                    let existing: Option<Object> =
                        query_as("SELECT * FROM datafiles WHERE scope = ?1 AND file_name = ?2")
                            .bind(scope)
                            .bind(filename)
                            .fetch_optional(tx.as_mut())
                            .await?;

                    match existing {
                        Some(file) if file.file_bytes.to_vec() == payload => {
                            if !inlined {
                                let _ = fs::remove_file(&tmp).await;
                                tx.rollback().await?;
                            }

                            debug!("put: identical file already exists");
                            return Ok(());
                        }
                        Some(_) => {
                            warn!("put: file already exists with different content");
                        }
                        None => {
                            // unique_violation but row doesn't exist - shouldn't happen
                            warn!("corrupted! 'is_unique_violation' received, but data cannot be selected");
                        }
                    }

                    Err(StorageError::AlreadyExists(
                        scope.to_string(),
                        filename.to_string(),
                    ))
                }
                Err(e) => Err(e.into()),
            }
        }.await;

        if result.is_err() && !inlined {
            let _ = fs::remove_file(&tmp).await;
        }

        result
    }

    /// Insert or update releases with their files in a single transaction.
    pub async fn insert_releases(
        &self,
        releases: &[(RawRelease, Vec<RawReleaseFile>)],
    ) -> Result<(), StorageError> {
        for (release, files) in releases {
            let mut tx = match self.sqlite.begin().await {
                Ok(tx) => tx,
                Err(e) => {
                    error!(
                        backend = release.backend,
                        version = release.version,
                        "failed to start transaction: {e}"
                    );
                    return Err(e.into());
                }
            };

            let meta_json = release
                .meta
                .as_ref()
                .map(|m| serde_json::to_vec(m).unwrap());

            let result = async {
                // Insert/update version
                query(
                    "INSERT INTO versions (backend, version, sort_key, meta)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(backend, version) DO UPDATE SET
                         sort_key = excluded.sort_key,
                         meta = excluded.meta
                     WHERE sort_key IS NOT excluded.sort_key OR meta IS NOT excluded.meta",
                )
                .bind(&release.backend)
                .bind(&release.version)
                .bind(release.sort_key)
                .bind(meta_json.as_deref())
                .execute(&mut *tx)
                .await?;

                // Insert/update files
                for file in files {
                    let changed =
                        Self::insert_file(&mut tx, &release.backend, &release.version, file)
                            .await?;
                    if changed {
                        warn!(
                            backend = release.backend,
                            version = release.version,
                            filename = file.filename,
                            "index file changed"
                        );
                    }
                }

                Ok::<(), StorageError>(())
            }
            .await;

            match result {
                Ok(()) => {
                    if let Err(e) = tx.commit().await {
                        error!(
                            backend = release.backend,
                            version = release.version,
                            "failed to commit release: {e}"
                        );
                    }
                }
                Err(e) => {
                    error!(
                        backend = release.backend,
                        version = release.version,
                        "failed to store release, skipping: {e}"
                    );
                    let _ = tx.rollback().await;
                }
            }
        }

        Ok(())
    }

    /// Insert or update a single file within a transaction.
    /// Returns `true` if an existing file was modified.
    async fn insert_file(
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        backend: &str,
        version: &str,
        file: &RawReleaseFile,
    ) -> Result<bool, StorageError> {
        let meta_json = file.meta.as_ref().map(|m| serde_json::to_vec(m).unwrap());
        let os_str = file.os.as_ref().map(AsRef::as_ref);
        let arch_str = file.arch.as_ref().map(AsRef::as_ref);

        let existed: Option<(i32,)> =
            query_as("SELECT 1 FROM files WHERE backend = ?1 AND version = ?2 AND filename = ?3")
                .bind(backend)
                .bind(version)
                .bind(&file.filename)
                .fetch_optional(&mut **tx)
                .await?;

        let changed: Option<(i32,)> = query_as(
            "INSERT INTO files (backend, version, filename, checksum, size, os, arch, meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(backend, version, filename) DO UPDATE SET
                 checksum = excluded.checksum,
                 size = excluded.size,
                 os = excluded.os,
                 arch = excluded.arch,
                 meta = excluded.meta
             WHERE checksum IS NOT excluded.checksum
                OR size IS NOT excluded.size
                OR os IS NOT excluded.os
                OR arch IS NOT excluded.arch
                OR meta IS NOT excluded.meta
             RETURNING 1",
        )
        .bind(backend)
        .bind(version)
        .bind(&file.filename)
        .bind(&file.checksum)
        .bind(file.size)
        .bind(os_str)
        .bind(arch_str)
        .bind(meta_json.as_deref())
        .fetch_optional(&mut **tx)
        .await?;

        Ok(existed.is_some() && changed.is_some())
    }

    /// Execute a file query.
    pub async fn query_files_filtered(
        &self,
        q: FileQuery<'_>,
    ) -> Result<Vec<RawReleaseFile>, StorageError> {
        let sql = q.build_sql();

        let mut query = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                i64,
                Option<String>,
                Option<String>,
                Option<Vec<u8>>,
            ),
        >(&sql)
        .bind(q.backend);

        if let Some(v) = q.version {
            query = query.bind(v);
        }
        if let Some(f) = q.filename {
            query = query.bind(f);
        }

        let rows = query.fetch_all(&self.sqlite).await?;

        rows.into_iter()
            .map(
                |(backend, version, filename, checksum, size, os_str, arch_str, meta_bytes)| {
                    let meta = meta_bytes
                        .map(|b| serde_json::from_slice(&b))
                        .transpose()
                        .map_err(|e| StorageError::DbError(sqlx::Error::Decode(Box::new(e))))?;

                    Ok(RawReleaseFile {
                        backend,
                        version,
                        filename,
                        checksum,
                        size,
                        os: os_str.and_then(|s| s.parse().ok()),
                        arch: arch_str.and_then(|s| s.parse().ok()),
                        meta,
                    })
                },
            )
            .collect()
    }

    /// Query all versions for a backend, ordered by sort_key.
    pub async fn query_releases(&self, backend: &str) -> Result<Vec<RawRelease>, StorageError> {
        let rows = query_as::<_, (String, String, i64, Option<Vec<u8>>)>(
            "SELECT backend, version, sort_key, meta FROM versions WHERE backend = ?1 ORDER BY sort_key",
        )
        .bind(backend)
        .fetch_all(&self.sqlite)
        .await?;

        rows.into_iter()
            .map(|(backend, version, sort_key, meta_bytes)| {
                let meta = meta_bytes
                    .map(|b| serde_json::from_slice(&b))
                    .transpose()
                    .map_err(|e| StorageError::DbError(sqlx::Error::Decode(Box::new(e))))?;

                Ok(Release {
                    backend,
                    version,
                    sort_key,
                    meta,
                })
            })
            .collect()
    }

    /// Update a single file entry.
    pub async fn update_file(&self, file: &RawReleaseFile) -> Result<bool, StorageError> {
        let mut tx = self.sqlite.begin().await?;
        let changed = Self::insert_file(&mut tx, &file.backend, &file.version, file).await?;
        tx.commit().await?;
        Ok(changed)
    }
}

#[async_trait::async_trait]
impl BackendStorage for StorageService {
    async fn insert_releases(
        &self,
        releases: &[(RawRelease, Vec<RawReleaseFile>)],
    ) -> Result<(), BackendError> {
        StorageService::insert_releases(self, releases)
            .await
            .map_err(|e| BackendError::Storage(e.to_string()))
    }

    async fn query_releases(&self, backend: &str) -> Result<Vec<RawRelease>, BackendError> {
        StorageService::query_releases(self, backend)
            .await
            .map_err(|e| BackendError::Storage(e.to_string()))
    }

    async fn query_files(
        &self,
        backend: &str,
        version: Option<&str>,
        filename: Option<&str>,
        meta_null_field: Option<&str>,
    ) -> Result<Vec<RawReleaseFile>, BackendError> {
        let mut q = FileQuery::new(backend);
        if let Some(v) = version {
            q = q.version(v);
        }
        if let Some(f) = filename {
            q = q.filename(f);
        }
        if let Some(field) = meta_null_field {
            q = q.where_meta_null(field);
        }
        self.query_files_filtered(q)
            .await
            .map_err(|e| BackendError::Storage(e.to_string()))
    }

    async fn update_file(&self, file: &RawReleaseFile) -> Result<bool, BackendError> {
        StorageService::update_file(self, file)
            .await
            .map_err(|e| BackendError::Storage(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const BIG_FILE_SIZE: usize = INLINE_THRESHOLD * 64;

    async fn create_storage_at(path: &Path) -> StorageService {
        let config = sync::Arc::new(ConfigService::for_test(path.to_path_buf()));
        StorageService::new(config).await.unwrap()
    }

    async fn create_test_storage() -> (StorageService, TempDir) {
        let tmp = TempDir::new().unwrap();
        let storage = create_storage_at(tmp.path()).await;
        (storage, tmp)
    }

    /// Count files in a directory recursively
    fn count_files(dir: &Path, ext: Option<&str>) -> usize {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    count += count_files(&path, ext);
                } else if let Some(extension) = ext {
                    if path.extension().is_some_and(|ext| ext == extension) {
                        count += 1;
                    }
                } else {
                    count += 1;
                }
            }
        }
        count
    }

    /// Recursively delete or corrupt all files in a directory
    fn damage_blobs(dir: &Path, corrupt: bool) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    damage_blobs(&path, corrupt);
                } else if corrupt {
                    if let Ok(meta) = path.metadata() {
                        let _ = std::fs::write(&path, vec![0x00; meta.len() as usize]);
                    }
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    #[tokio::test]
    async fn test_write_read_small_file() {
        let (storage, _tmp) = create_test_storage().await;

        // 1 KB file - should be stored inline
        let data = Bytes::from(vec![0xAB; 1024]);
        storage.put("test-scope", "small.bin", &data).await.unwrap();

        let file = storage.get("test-scope", "small.bin").await.unwrap();
        assert!(file.is_some());

        let file = file.unwrap();
        assert!(file.inlined);
        assert_eq!(file.file_bytes.0, data);
        assert_eq!(file.file_size, 1024);
    }

    #[tokio::test]
    async fn test_write_read_large_file() {
        let (storage, tmp) = create_test_storage().await;

        // Large file - should be stored on disk
        let data = Bytes::from(vec![0xCD; BIG_FILE_SIZE]);
        storage.put("test-scope", "large.bin", &data).await.unwrap();

        let file = storage.get("test-scope", "large.bin").await.unwrap();
        assert!(file.is_some());

        let file = file.unwrap();
        assert!(!file.inlined);
        assert_eq!(file.file_bytes.0, data);
        assert_eq!(file.file_size, (BIG_FILE_SIZE) as i64);

        // Verify file is stored on disk, not inline in SQLite
        let db_size = std::fs::metadata(tmp.path().join("index.sqlite"))
            .unwrap()
            .len();
        assert!(
            db_size < data.len() as u64,
            "database ({db_size} bytes) should be smaller than file ({} bytes)",
            data.len()
        );
    }

    #[tokio::test]
    async fn test_write_read_boundary_file() {
        let (storage, _tmp) = create_test_storage().await;

        // Exactly 256 KB - should be stored inline (threshold is <=)
        let data = Bytes::from(vec![0xEF; INLINE_THRESHOLD]);
        storage
            .put("test-scope", "boundary.bin", &data)
            .await
            .unwrap();

        let file = storage.get("test-scope", "boundary.bin").await.unwrap();
        assert!(file.is_some());

        let file = file.unwrap();
        assert!(file.inlined);
        assert_eq!(file.file_bytes.0, data);
        assert_eq!(file.file_size, INLINE_THRESHOLD as i64);
    }

    #[tokio::test]
    async fn test_read_nonexistent_file() {
        let (storage, _tmp) = create_test_storage().await;

        let file = storage.get("test-scope", "nonexistent.bin").await.unwrap();
        assert!(file.is_none());
    }

    #[tokio::test]
    async fn test_persistence_after_restart() {
        let tmp = TempDir::new().unwrap();

        let small_data = Bytes::from(vec![0x11; 1024]);
        let large_data = Bytes::from(vec![0x22; BIG_FILE_SIZE]);

        // First "session": write files
        {
            let storage = create_storage_at(tmp.path()).await;
            storage
                .put("persist", "small.bin", &small_data)
                .await
                .unwrap();
            storage
                .put("persist", "large.bin", &large_data)
                .await
                .unwrap();
            // storage is dropped here, simulating shutdown
        }

        // Second "session": verify files persist
        {
            let storage = create_storage_at(tmp.path()).await;

            let small = storage.get("persist", "small.bin").await.unwrap();
            assert!(small.is_some());
            assert_eq!(small.unwrap().file_bytes.0, small_data);

            let large = storage.get("persist", "large.bin").await.unwrap();
            assert!(large.is_some());
            assert_eq!(large.unwrap().file_bytes.0, large_data);
        }
    }

    #[tokio::test]
    async fn test_error_blob_not_found() {
        let (storage, tmp) = create_test_storage().await;

        // Write a large file (stored on disk)
        let data = Bytes::from(vec![0xAA; BIG_FILE_SIZE]);
        storage
            .put("test-scope", "to-delete.bin", &data)
            .await
            .unwrap();

        // Delete all blob files from disk
        damage_blobs(&tmp.path().join("objects"), false);

        // Reading should fail with BlobNotFound
        let result = storage.get("test-scope", "to-delete.bin").await;
        assert!(matches!(result, Err(StorageError::BlobNotFound(_))));
    }

    #[tokio::test]
    async fn test_error_blob_corrupted() {
        let (storage, tmp) = create_test_storage().await;

        // Write a large file (stored on disk)
        let data = Bytes::from(vec![0xBB; BIG_FILE_SIZE]);
        storage
            .put("test-scope", "to-corrupt.bin", &data)
            .await
            .unwrap();

        // Corrupt all blob files on disk
        damage_blobs(&tmp.path().join("objects"), true);

        // Reading should fail with IntegrityError
        let result = storage.get("test-scope", "to-corrupt.bin").await;
        assert!(matches!(result, Err(StorageError::IntegrityError)));
    }

    #[tokio::test]
    async fn test_duplicate_put_same_content() {
        let (storage, tmp) = create_test_storage().await;

        // Write a large file
        let data = Bytes::from(vec![0xCC; BIG_FILE_SIZE]);
        storage.put("test-scope", "dup.bin", &data).await.unwrap();

        // Write the same file again with identical content - should succeed
        let result = storage.put("test-scope", "dup.bin", &data).await;
        assert!(result.is_ok());

        // Verify no temp files left behind
        assert_eq!(count_files(&tmp.path().join("objects"), Some("part")), 0);

        // Verify file is still readable with correct content
        let file = storage.get("test-scope", "dup.bin").await.unwrap();
        assert!(file.is_some());
        assert_eq!(file.unwrap().file_bytes.0, data);
    }

    #[tokio::test]
    async fn test_duplicate_put_different_content() {
        let (storage, tmp) = create_test_storage().await;

        // Write a large file
        let data1 = Bytes::from(vec![0xDD; BIG_FILE_SIZE]);
        storage.put("test-scope", "dup.bin", &data1).await.unwrap();

        // Write the same filename with different content - should fail
        let data2 = Bytes::from(vec![0xEE; BIG_FILE_SIZE]);
        let result = storage.put("test-scope", "dup.bin", &data2).await;
        assert!(matches!(result, Err(StorageError::AlreadyExists(_, _))));

        // Verify no temp files left behind
        assert_eq!(count_files(&tmp.path().join("objects"), Some("part")), 0);

        // Verify original file is still intact
        let file = storage.get("test-scope", "dup.bin").await.unwrap();
        assert!(file.is_some());
        assert_eq!(file.unwrap().file_bytes.0, data1);
    }

    #[tokio::test]
    async fn test_doctor_removes_temp_files() {
        let tmp = TempDir::new().unwrap();

        // First session: write a large file
        let data = Bytes::from(vec![0xAA; BIG_FILE_SIZE]);
        {
            let storage = create_storage_at(tmp.path()).await;
            storage.put("test-scope", "valid.bin", &data).await.unwrap();
        }

        // Manually create temp files in the objects directory
        let objects_dir = tmp.path().join("objects");
        std::fs::create_dir_all(objects_dir.join("ab/cd")).unwrap();
        std::fs::write(objects_dir.join("ab/cd/test.part"), b"temp1").unwrap();
        std::fs::write(objects_dir.join("ab/cd/another.12345.part"), b"temp2").unwrap();

        assert_eq!(count_files(&objects_dir, Some("part")), 2);

        // Second session: doctor should clean up temp files
        {
            let storage = create_storage_at(tmp.path()).await;

            // Temp files should be gone
            assert_eq!(count_files(&objects_dir, Some("part")), 0);

            // Valid file should still be readable
            let file = storage.get("test-scope", "valid.bin").await.unwrap();
            assert!(file.is_some());
            assert_eq!(file.unwrap().file_bytes.0, data);
        }
    }

    #[tokio::test]
    async fn test_doctor_removes_orphan_blobs() {
        let tmp = TempDir::new().unwrap();

        // First session: write a large file
        let data = Bytes::from(vec![0xBB; BIG_FILE_SIZE]);
        {
            let storage = create_storage_at(tmp.path()).await;
            storage.put("test-scope", "valid.bin", &data).await.unwrap();
        }

        // Count files before adding orphan
        let objects_dir = tmp.path().join("objects");
        let files_before = count_files(&objects_dir, None);
        assert_eq!(files_before, 1); // Only the valid blob

        // Manually create an orphan blob (valid hex name but no DB record)
        std::fs::create_dir_all(objects_dir.join("de/ad")).unwrap();
        let orphan_name = "deadbeefdeadbeefdeadbeefdeadbeef"; // 32 hex chars
        std::fs::write(objects_dir.join("de/ad").join(orphan_name), b"orphan").unwrap();

        assert_eq!(count_files(&objects_dir, None), 2);

        // Second session: doctor should clean up orphan blob
        {
            let storage = create_storage_at(tmp.path()).await;

            // Only the valid file should remain
            assert_eq!(count_files(&objects_dir, None), 1);

            // Valid file should still be readable
            let file = storage.get("test-scope", "valid.bin").await.unwrap();
            assert!(file.is_some());
            assert_eq!(file.unwrap().file_bytes.0, data);
        }
    }
}
