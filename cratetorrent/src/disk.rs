mod error;
mod io;

pub use error::*;

use {
    crate::{torrent::StorageInfo, BlockInfo, TorrentId},
    io::Disk,
    tokio::{
        sync::mpsc::{UnboundedReceiver, UnboundedSender},
        task,
    },
};

/// Spawns a disk IO task and returns a tuple with the task join handle, the
/// disk handle used for sending commands, and a channel for receiving
/// command results and other notifications.
pub(crate) fn spawn(
) -> Result<(task::JoinHandle<Result<()>>, DiskHandle, AlertReceiver)> {
    log::info!("Spawning disk IO task");
    let (mut disk, cmd_chan, alert_port) = Disk::new()?;
    // spawn disk event loop on a new task
    let join_handle = task::spawn(async move { disk.start().await });
    log::info!("Spawned disk IO task");

    Ok((join_handle, DiskHandle(cmd_chan), alert_port))
}

/// The handle for the disk task, used to execute disk IO related tasks.
///
/// The handle may be copied an arbitrary number of times. It is an abstraction
/// over the means to communicate with the disk IO task. For now, mpsc channels
/// are used for issuing commands and receiving results, but this may well
/// change later on, hence hiding this behind this handle type.
#[derive(Clone)]
pub(crate) struct DiskHandle(CommandSender);

impl DiskHandle {
    /// Instructs the disk task to set up everything needed for a new torrent,
    /// which includes in-memory metadata storage and pre-allocating the
    /// to-be-downloaded file(s).
    pub fn allocate_new_torrent(
        &self,
        id: TorrentId,
        info: StorageInfo,
        piece_hashes: Vec<u8>,
    ) -> Result<()> {
        log::trace!("Allocating new torrent {}", id);
        self.0
            .send(Command::NewTorrent {
                id,
                info,
                piece_hashes,
            })
            .map_err(Error::from)
    }

    /// Queues a block for eventual writing to disk.
    ///
    /// Once the block is saved, the result is advertised to its
    /// `AlertReceiver`.
    pub fn write_block(
        &self,
        id: TorrentId,
        info: BlockInfo,
        data: Vec<u8>,
    ) -> Result<()> {
        log::trace!("Saving block {:?} to disk", info);
        self.0
            .send(Command::WriteBlock { id, info, data })
            .map_err(Error::from)
    }

    /// Shuts down the disk IO task.
    pub fn shutdown(&self) -> Result<()> {
        log::trace!("Shutting down disk IO task");
        self.0.send(Command::Shutdown).map_err(Error::from)
    }
}

/// The channel for sendng commands to the disk task.
type CommandSender = UnboundedSender<Command>;
/// The channel the disk task uses to listen for commands.
type CommandReceiver = UnboundedReceiver<Command>;

/// The type of commands that the disk can execute.
enum Command {
    // Allocate a new torrent.
    NewTorrent {
        id: TorrentId,
        info: StorageInfo,
        piece_hashes: Vec<u8>,
    },
    // Request to eventually write a block to disk.
    WriteBlock {
        id: TorrentId,
        info: BlockInfo,
        data: Vec<u8>,
    },
    // Eventually shut down the disk task.
    Shutdown,
}

/// The type of channel used to alert the engine about global events.
type AlertSender = UnboundedSender<Alert>;
/// The channel on which the engine can listen for global disk events.
pub(crate) type AlertReceiver = UnboundedReceiver<Alert>;

/// The alerts that the disk task may send about global events (i.e.  events not
/// related to individual torrents).
#[derive(Debug)]
pub(crate) enum Alert {
    /// Torrent allocation result. If successful, the id of the allocated
    /// torrent is returned for identification, if not, the reason of the error
    /// is included.
    TorrentAllocation(Result<TorrentAllocation, NewTorrentError>),
}

/// The result of successfully allocating a torrent.
#[derive(Debug)]
pub(crate) struct TorrentAllocation {
    /// The id of the torrent that has been allocated.
    pub id: TorrentId,
    /// The port on which torrent may receive alerts.
    pub alert_port: TorrentAlertReceiver,
}

/// The type of channel used to alert a torrent about torrent specific events.
type TorrentAlertSender = UnboundedSender<TorrentAlert>;
/// The type of channel on which a torrent can listen for block write
/// completions.
pub(crate) type TorrentAlertReceiver = UnboundedReceiver<TorrentAlert>;

/// The alerts that the disk task may send about events related to a specific
/// torrent.
#[derive(Debug)]
pub(crate) enum TorrentAlert {
    /// Sent when some blocks were written to disk or an error ocurred while
    /// writing.
    BatchWrite(Result<BatchWrite, WriteError>),
}

/// Type returned on each successful batch of blocks written to disk.
#[derive(Debug)]
pub(crate) struct BatchWrite {
    /// The piece blocks that were written to the disk in this batch.
    ///
    /// There is some inefficiency in having the piece index in all blocks,
    /// however, this allows for supporting alerting writes of blocks in
    /// multiple pieces, which is a feature for later (and for now this is kept
    /// for simplicity).
    ///
    /// If the piece is invalid, this vector is empty.
    pub blocks: Vec<BlockInfo>,
    /// This field is set for the block write that completes the piece and
    /// contains whether the downloaded piece's hash matches its expected hash.
    pub is_piece_valid: Option<bool>,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{block_count, BLOCK_LEN},
        sha1::{Digest, Sha1},
        std::{fs, path::PathBuf},
    };

    // Tests the allocation of a torrent, and then the allocation of the same
    // torrent returning an error.
    #[tokio::test]
    async fn test_allocate_new_torrent() {
        let (_, disk_handle, mut alert_port) = spawn().unwrap();

        let Env {
            id,
            pieces,
            piece_hashes,
            info,
        } = Env::new();

        // allocate torrent via channel
        disk_handle
            .allocate_new_torrent(id, info, piece_hashes.clone())
            .unwrap();

        // wait for result on alert port
        let alert = alert_port.recv().await.unwrap();
        match alert {
            Alert::TorrentAllocation(Ok(allocation)) => {
                assert_eq!(allocation.id, id);
            }
            _ => {
                assert!(false, "Torrent could not be allocated");
            }
        }

        // try to allocate the same torrent a second time
        disk_handle
            .allocate_new_torrent(id, info, piece_hashes)
            .unwrap();

        // we should get an already exists error
        let alert = alert_port.recv().await.unwrap();
        assert!(matches!(
            alert,
            Alert::TorrentAllocation(Err(NewTorrentError::AlreadyExists))
        ));
    }

    // Tests writing of a complete valid torrent's pieces and verifying that an
    // alert of each disk write is returned by the disk task.
    #[tokio::test]
    async fn test_write_all_pieces() {
        let (_, disk_handle, mut alert_port) = spawn().unwrap();

        let Env {
            id,
            pieces,
            piece_hashes,
            info,
        } = Env::new();

        // allocate torrent via channel
        disk_handle
            .allocate_new_torrent(id, info, piece_hashes)
            .unwrap();

        // wait for result on alert port
        let mut torrent_disk_alert_port =
            if let Some(Alert::TorrentAllocation(Ok(allocation))) =
                alert_port.recv().await
            {
                allocation.alert_port
            } else {
                assert!(false, "Torrent could not be allocated");
                return;
            };

        // write all pieces to disk
        for index in 0..pieces.len() {
            let piece = &pieces[index];
            for_each_block(index, piece.len() as u32, |info| {
                let block_end = info.offset + info.len;
                let data = &piece[info.offset as usize..block_end as usize];
                debug_assert_eq!(data.len(), info.len as usize);
                println!("Writing piece {} block {:?}", index, info);
                disk_handle.write_block(id, info, data.to_vec()).unwrap();
            });

            // wait for disk write result
            if let Some(TorrentAlert::BatchWrite(Ok(batch))) =
                torrent_disk_alert_port.recv().await
            {
                // piece is complete so it should be hashed and be valid
                assert!(matches!(batch.is_piece_valid, Some(true)));
                // verify that the message contains all four blocks
                for_each_block(index, piece.len() as u32, |info| {
                    let pos = batch.blocks.iter().position(|b| *b == info);
                    println!("Verifying piece {} block {:?}", index, info);
                    assert!(pos.is_some());
                });
            } else {
                assert!(false, "Piece could not be written to disk");
            }
        }

        // clean up test env
        fs::remove_file(&info.download_path)
            .expect("Failed to clean up disk test torrent file");
    }

    // Calls the provided function for each block in piece, passing it the
    // block's `BlockInfo`.
    fn for_each_block(
        piece_index: usize,
        piece_len: u32,
        block_visitor: impl Fn(BlockInfo),
    ) {
        let block_count = block_count(piece_len) as u32;
        // all pieces have four blocks in this test
        debug_assert_eq!(block_count, 4);

        let mut block_offset = 0;
        for _ in 0..block_count {
            // when calculating the block length we need to consider that the
            // last block may be smaller than the rest
            let block_len = (piece_len - block_offset).min(BLOCK_LEN);
            debug_assert!(block_len > 0);
            debug_assert!(block_len <= BLOCK_LEN);

            block_visitor(BlockInfo {
                piece_index,
                offset: block_offset,
                len: block_len,
            });

            // increment offset for next piece
            block_offset += block_len;
        }
    }

    // Tests writing of an invalid piece and verifying that an alert the invalid
    // disk is returned by the disk task.
    #[tokio::test]
    async fn test_write_invalid_piece() {
        let (_, disk_handle, mut alert_port) = spawn().unwrap();

        let Env {
            id,
            pieces,
            piece_hashes,
            info,
        } = Env::new();

        // allocate torrent via channel
        disk_handle
            .allocate_new_torrent(id, info, piece_hashes)
            .unwrap();

        // wait for result on alert port
        let mut torrent_disk_alert_port =
            if let Some(Alert::TorrentAllocation(Ok(allocation))) =
                alert_port.recv().await
            {
                allocation.alert_port
            } else {
                assert!(false, "Torrent could not be allocated");
                return;
            };

        // write an invalid piece to disk
        let index = 0;
        let invalid_piece: Vec<_> =
            pieces[index].iter().map(|b| b.saturating_add(5)).collect();
        for_each_block(index, invalid_piece.len() as u32, |info| {
            let block_end = info.offset + info.len;
            let data = &invalid_piece[info.offset as usize..block_end as usize];
            debug_assert_eq!(data.len(), info.len as usize);
            println!("Writing invalid piece {} block {:?}", index, info);
            disk_handle.write_block(id, info, data.to_vec()).unwrap();
        });

        // wait for disk write result
        if let Some(TorrentAlert::BatchWrite(Ok(batch))) =
            torrent_disk_alert_port.recv().await
        {
            // piece is complete so it should be hashed but be invalid
            assert!(matches!(batch.is_piece_valid, Some(false)));
            // verify that the message doesn't contain any blocks
            assert!(batch.blocks.is_empty());
        } else {
            assert!(false, "Piece could not be written to disk");
        }

        // download file should not exists as invalid piece must not be written
        // to disk
        assert!(!info.download_path.exists());
    }

    // The disk IO test environment containing information of a valid torrent.
    struct Env {
        id: TorrentId,
        pieces: Vec<Vec<u8>>,
        piece_hashes: Vec<u8>,
        info: StorageInfo,
    }

    impl Env {
        fn new() -> Self {
            let id = 0;
            let download_path = PathBuf::from("/tmp/torrent0");
            let piece_len: u32 = 4 * 0x4000;
            // last piece is slightly shorter to test that it is handled correctly
            let last_piece_len: u32 = piece_len - 935;
            let pieces: Vec<Vec<u8>> = vec![
                (0..piece_len).map(|b| (b % 256) as u8).collect(),
                (0..piece_len)
                    .map(|b| b + 1)
                    .map(|b| (b % 256) as u8)
                    .collect(),
                (0..piece_len)
                    .map(|b| b + 2)
                    .map(|b| (b % 256) as u8)
                    .collect(),
                (0..last_piece_len)
                    .map(|b| b + 3)
                    .map(|b| (b % 256) as u8)
                    .collect(),
            ];
            // build up expected piece hashes
            let mut piece_hashes = Vec::with_capacity(pieces.len() * 20);
            for piece in pieces.iter() {
                let hash = Sha1::digest(&piece);
                piece_hashes.extend(hash.as_slice());
            }
            assert_eq!(piece_hashes.len(), pieces.len() * 20);

            // clean up any potential previous test env
            if download_path.exists() {
                fs::remove_file(&download_path)
                    .expect("Failed to clean up disk test torrent file");
            }

            let info = StorageInfo {
                piece_count: pieces.len(),
                piece_len,
                last_piece_len,
                download_len: pieces.iter().fold(0, |mut len, piece| {
                    len += piece.len() as u64;
                    len
                }),
                download_path,
            };

            Self {
                id,
                pieces,
                piece_hashes,
                info,
            }
        }
    }
}