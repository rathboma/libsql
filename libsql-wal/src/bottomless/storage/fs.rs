use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::AsyncBufRead;

use crate::bottomless::job::CompactedSegmentDataHeader;
use crate::bottomless::{Error, Result};
use crate::io::{FileExt, Io};
use libsql_sys::name::NamespaceName;

use super::{SegmentMeta, Storage};

pub struct FsStorage<I, S> {
    prefix: PathBuf,
    io: Arc<I>,
    remote_storage: Arc<S>,
}

impl<I: Io, S> FsStorage<I, S> {
    fn new(prefix: PathBuf, io: I, remote_storage: S) -> Result<Self> {
        io.create_dir_all(&prefix.join("segments")).unwrap();

        Ok(FsStorage {
            prefix,
            io: Arc::new(io),
            remote_storage: Arc::new(remote_storage),
        })
    }
}

pub(crate) trait RemoteStorage: Send + Sync + 'static {
    type FetchStream: AsyncBufRead;

    fn upload(
        &self,
        file_path: &Path,
        meta: &SegmentMeta,
    ) -> impl Future<Output = Result<()>> + Send;

    fn fetch(
        &self,
        namespace: &NamespaceName,
        frame_no: u64,
    ) -> impl Future<Output = Result<Self::FetchStream>> + Send;
}

impl RemoteStorage for () {
    type FetchStream = tokio::io::Empty;

    async fn upload(&self, _file_path: &Path, _meta: &SegmentMeta) -> Result<()> {
        Ok(())
    }

    async fn fetch(&self, _namespace: &NamespaceName, frame_no: u64) -> Result<Self::FetchStream> {
        Err(Error::FrameNotFound(frame_no))
    }
}

// TODO(lucio): handle errors for fs module
impl<I: Io, S: RemoteStorage> Storage for FsStorage<I, S> {
    type Config = ();

    async fn store(
        &self,
        config: &Self::Config,
        meta: super::SegmentMeta,
        segment_data: impl crate::io::file::FileExt,
        segment_index: Vec<u8>,
    ) -> Result<()> {
        let key = format!(
            "{:019}-{:019}-{:019}.segment",
            meta.start_frame_no,
            meta.end_frame_no,
            meta.created_at.timestamp()
        );

        let path = self.prefix.join("segments").join(&key);

        let buf = Vec::with_capacity(segment_data.len().unwrap() as usize);

        let f = self.io.open(true, false, true, &path).unwrap();
        let (buf, res) = segment_data.read_exact_at_async(buf, 0).await;

        let (_, res) = f.write_all_at_async(buf, 0).await;
        res?;

        self.remote_storage.upload(&path, &meta).await?;

        Ok(())
    }

    async fn fetch_segment(
        &self,
        _config: &Self::Config,
        namespace: NamespaceName,
        frame_no: u64,
        dest_path: &Path,
    ) -> Result<()> {
        // TODO(lucio): prefix also via namespace
        let dir = self.prefix.join("segments");

        // TODO(lucio): optimization would be to cache this list, since we update the files in the
        // store fn we can keep track without having to go to the OS each time.
        let mut dirs = tokio::fs::read_dir(dir).await?;

        while let Some(entry) = dirs.next_entry().await? {
            let file = entry.file_name();
            let key = file.to_str().unwrap().split(".").next().unwrap();
            let mut comp = key.split("-");

            let start_frame = comp.next().unwrap();
            let end_frame = comp.next().unwrap();

            let start_frame: u64 = start_frame.parse().unwrap();
            let end_frame: u64 = end_frame.parse().unwrap();

            if start_frame <= frame_no && end_frame >= frame_no {
                #[cfg(debug_assertions)]
                {
                    use crate::io::buf::ZeroCopyBuf;

                    let header_buf = ZeroCopyBuf::<CompactedSegmentDataHeader>::new_uninit();
                    let file = self
                        .io
                        .open(false, true, false, dbg!(&entry.path()))
                        .unwrap();
                    let (header_buf, res) = file.read_exact_at_async(header_buf, 0).await;
                    res.unwrap();

                    let header = header_buf.get_ref();
                    let start_frame_from_header = header.start_frame_no.get();
                    let end_frame_from_header = header.end_frame_no.get();

                    // TOOD(lucio): convert these into errors before prod
                    assert_eq!(start_frame, start_frame_from_header);
                    assert_eq!(end_frame, end_frame_from_header);
                }

                self.io.hard_link(&entry.path(), dest_path)?;

                return Ok(());
            }
        }

        // TODO(lucio): fetch from remote storage
        let out_folder = PathBuf::new();
        let reader = self.remote_storage.fetch(&namespace, frame_no).await?;

        // TODO(lucio): write buf reader content into the expected destination file then hard link

        // self.io.hard_link(&path, dest_path)?;

        Err(Error::Store("".into()))
    }

    async fn meta(
        &self,
        _config: &Self::Config,
        _namespace: NamespaceName,
    ) -> Result<super::DbMeta> {
        todo!()
    }

    fn default_config(&self) -> std::sync::Arc<Self::Config> {
        todo!()
    }
}

pub(super) fn parse_segment_file_name(name: &str) -> Result<(u64, u64)> {
    tracing::debug!("parsing file name: {}", name);
    let key = name.split(".").next().unwrap();
    let mut comp = key.split("-");

    let start_frame = comp.next().unwrap();
    let end_frame = comp.next().unwrap();

    let start_frame: u64 = start_frame.parse().unwrap();
    let end_frame: u64 = end_frame.parse().unwrap();

    Ok((start_frame, end_frame))
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use chrono::Utc;
    use tempfile::tempdir;
    use uuid::Uuid;
    use zerocopy::{AsBytes, FromZeroes};

    use super::*;
    use crate::{bottomless::Storage, io::StdIO};

    #[tokio::test]
    async fn read_write() {
        let dir = tempdir().unwrap();
        let fs = FsStorage::new(dir.path().into(), StdIO::default(), ()).unwrap();

        let namespace = NamespaceName::from_string("".into());
        let segment = CompactedSegmentDataHeader {
            start_frame_no: 0.into(),
            frame_count: 10.into(),
            segment_id: 0.into(),
            end_frame_no: 64.into(),
        };

        fs.store(
            &(),
            crate::bottomless::storage::SegmentMeta {
                namespace: namespace.clone(),
                segment_id: Uuid::new_v4(),
                start_frame_no: 0,
                end_frame_no: 64,
                created_at: Utc::now(),
            },
            segment.as_bytes().to_vec(),
            Vec::new(),
        )
        .await
        .unwrap();

        let path = dir.path().join("fetched_segment");
        fs.fetch_segment(&(), namespace.clone(), 5, &path)
            .await
            .unwrap();

        let mut file = std::fs::File::open(path).unwrap();
        let mut header: CompactedSegmentDataHeader = CompactedSegmentDataHeader::new_zeroed();

        file.read_exact(header.as_bytes_mut()).unwrap();

        assert_eq!(header.start_frame_no.get(), 0);
        assert_eq!(header.end_frame_no.get(), 64);
    }
}
