//! Virtual disk backends. Sectors are 256 bytes, addressed by LSN.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use drivewire_proto::Lsn;
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::Mutex;

pub const SECTOR_SIZE: usize = 256;
pub type Sector = [u8; SECTOR_SIZE];

#[derive(Debug, Error)]
pub enum VDiskError {
    #[error("sector {0:?} out of range")]
    OutOfRange(Lsn),
    #[error("read-only disk")]
    ReadOnly,
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait VDisk: Send + Sync {
    async fn read(&self, lsn: Lsn) -> Result<Sector, VDiskError>;
    async fn write(&self, lsn: Lsn, data: &Sector) -> Result<(), VDiskError>;
    fn sector_count(&self) -> u32;
    fn is_read_only(&self) -> bool {
        false
    }
    /// Human-readable identifier for `dw status` and logs (usually the
    /// backing file path).
    fn name(&self) -> String {
        String::from("<unnamed>")
    }
}

/// Flat raw `.dsk` image: `sector_count * 256` bytes, LSN 0 at offset 0.
pub struct DskFile {
    path: PathBuf,
    file: Mutex<File>,
    sector_count: u32,
    read_only: bool,
}

impl DskFile {
    pub async fn open(path: impl AsRef<Path>, read_only: bool) -> Result<Arc<Self>, VDiskError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(&path)
            .await?;
        let len = file.metadata().await?.len();
        let sector_count = (len / SECTOR_SIZE as u64) as u32;
        Ok(Arc::new(Self {
            path,
            file: Mutex::new(file),
            sector_count,
            read_only,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl VDisk for DskFile {
    async fn read(&self, lsn: Lsn) -> Result<Sector, VDiskError> {
        if lsn.0 >= self.sector_count {
            return Err(VDiskError::OutOfRange(lsn));
        }
        let mut buf = [0u8; SECTOR_SIZE];
        let mut f = self.file.lock().await;
        f.seek(SeekFrom::Start(lsn.0 as u64 * SECTOR_SIZE as u64))
            .await?;
        f.read_exact(&mut buf).await?;
        Ok(buf)
    }

    async fn write(&self, lsn: Lsn, data: &Sector) -> Result<(), VDiskError> {
        if self.read_only {
            return Err(VDiskError::ReadOnly);
        }
        if lsn.0 >= self.sector_count {
            return Err(VDiskError::OutOfRange(lsn));
        }
        let mut f = self.file.lock().await;
        f.seek(SeekFrom::Start(lsn.0 as u64 * SECTOR_SIZE as u64))
            .await?;
        f.write_all(data).await?;
        f.flush().await?;
        Ok(())
    }

    fn sector_count(&self) -> u32 {
        self.sector_count
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn name(&self) -> String {
        self.path.display().to_string()
    }
}
