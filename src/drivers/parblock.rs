/*
 * Copyright © 2018, Steve Smith <tarkasteve@gmail.com>
 *
 * This program is free software: you can redistribute it and/or
 * modify it under the terms of the GNU General Public License version
 * 3 as published by the Free Software Foundation.
 *
 * This program is distributed in the hope that it will be useful, but
 * WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

use std::cmp;
use std::fs::{create_dir_all, read_link};
use std::ops::Range;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use cfg_if::cfg_if;
use crossbeam_channel as cbc;
use libfs::{FileType, copy_node};
use log::{debug, error, info};
use blocking_threadpool::{Builder, ThreadPool};
use walkdir::WalkDir;

use crate::drivers::CopyDriver;
use crate::errors::{Result, XcpError};
use crate::operations::{CopyHandle, StatusUpdate, StatSender};
use crate::options::{ignore_filter, parse_ignore, Opts};
use libfs::{copy_file_offset, map_extents, merge_extents, probably_sparse};
use crate::progress;
use crate::utils::empty;

// ********************************************************************** //

const fn supported_platform() -> bool {
    cfg_if! {
        if #[cfg(
            any(target_os = "linux",
                target_os = "android",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "dragonfly",
                target_os = "macos",
            ))]
        {
            true
        } else {
            false
        }
    }
}


pub struct Driver {
    opts: Arc<Opts>,
}

impl CopyDriver for Driver {
    fn new(opts: Arc<Opts>) -> Result<Self> {
        if !supported_platform() {
            let msg = "The parblock driver is not currently supported on this OS.";
            error!("{}", msg);
            return Err(XcpError::UnsupportedOS(msg).into());
        }

        Ok(Self {
            opts
        })
    }

    fn copy_all(&self, sources: Vec<PathBuf>, dest: &Path) -> Result<()> {
        copy_all(sources, dest, &self.opts)
    }

    fn copy_single(&self, source: &Path, dest: &Path) -> Result<()> {
        copy_single_file(source, dest, &self.opts)
    }
}

// ********************************************************************** //


struct CopyOp {
    from: PathBuf,
    target: PathBuf,
}

fn queue_file_range(
    handle: &Arc<CopyHandle>,
    range: Range<u64>,
    pool: &ThreadPool,
    status_channel: &StatSender,
) -> Result<u64> {
    let len = range.end - range.start;
    let bsize = handle.opts.block_size;
    let blocks = (len / bsize) + (if len % bsize > 0 { 1 } else { 0 });

    for blkn in 0..blocks {
        let harc = handle.clone();
        let stat_tx = status_channel.clone();
        let bytes = cmp::min(len - (blkn * bsize), bsize);
        let off = range.start + (blkn * bsize);

        pool.execute(move || {
            let r = copy_file_offset(&harc.infd, &harc.outfd, bytes, off as i64);
            match r {
                Ok(bytes) => {
                    stat_tx.send(StatusUpdate::Copied(bytes as u64)).unwrap();
                }
                Err(e) => {
                    stat_tx.send(StatusUpdate::Error(XcpError::CopyError(e.to_string()))).unwrap();
                    error!("Error copying: aborting.");
                }
            }
        });
    }
    Ok(len)
}

fn queue_file_blocks(
    source: &Path,
    dest: &Path,
    pool: &ThreadPool,
    status_channel: &StatSender,
    opts: &Arc<Opts>,
) -> Result<u64> {
    let handle = CopyHandle::new(source, dest, opts)?;
    let len = handle.metadata.len();

    if handle.try_reflink()? {
        info!("Reflinked, skipping rest of copy");
        return Ok(len);
    }

    // Put the open files in an Arc, which we drop once work has been
    // queued. This will keep the files open until all work has been
    // consumed, then close them. (This may be overkill; opening the
    // files in the workers would also be valid.)
    let harc = Arc::new(handle);

    let queue_whole_file = || {
        queue_file_range(&harc, 0..len, pool, status_channel)
    };

    if probably_sparse(&harc.infd)? {
        if let Some(extents) = map_extents(&harc.infd)? {
            let sparse_map = merge_extents(extents)?;
            let mut queued = 0;
            for ext in sparse_map {
                queued += queue_file_range(&harc, ext.into(), pool, status_channel)?;
            }
            Ok(queued)
        } else {
            queue_whole_file()
        }
    } else {
        queue_whole_file()
    }
}

fn copy_single_file(source: &Path, dest: &Path, opts: &Arc<Opts>) -> Result<()> {
    let nworkers = opts.num_workers();
    let pool = ThreadPool::new(nworkers as usize);

    let len = source.metadata()?.len();
    let pb = progress::create_bar(&opts, len)?;

    let (stat_tx, stat_rx) = cbc::unbounded();
    let sender = StatSender::new(stat_tx, &opts);
    queue_file_blocks(source, dest, &pool, &sender, opts)?;

    // Gather the results as we go; close our end of the channel so it
    // ends when drained.
    drop(sender);
    for stat in stat_rx {
        match stat {
            StatusUpdate::Copied(v) => pb.inc(v),
            StatusUpdate::Size(v) => pb.inc_size(v),
            StatusUpdate::Error(e) => {
                // FIXME: Optional continue?
                error!("Received error: {}", e);
                return Err(e.into());
            }
        }
    }

    pool.join();
    pb.end();

    Ok(())
}

// Dispatch worker; receives queued files and hands them to
// queue_file_blocks() which splits them onto the copy-pool.
fn dispatch_worker(file_q: cbc::Receiver<CopyOp>, stat_q: StatSender, opts: Arc<Opts>) -> Result<()> {
    let nworkers = opts.num_workers() as usize;
    let copy_pool = Builder::new()
        .num_threads(nworkers)
        // Use bounded queue for backpressure; this limits open
        // files in-flight so we don't run out of file handles.
        // FIXME: Number is arbitrary ATM, we should be able to
        // calculate it from ulimits.
        .queue_len(128)
        .build();
    for op in file_q {
        let r = queue_file_blocks(&op.from, &op.target, &copy_pool, &stat_q, &opts);
        if let Err(e) = r {
            stat_q.send(StatusUpdate::Error(XcpError::CopyError(e.to_string())))?;
            error!("Dispatcher: Error copying {:?} -> {:?}.", op.from, op.target);
            return Err(e)
        }
    }
    info!("Queuing complete");

    copy_pool.join();
    info!("Pool complete");

    Ok(())
}

fn copy_all(sources: Vec<PathBuf>, dest: &Path, opts: &Arc<Opts>) -> Result<()> {
    let pb = progress::create_bar(&opts, 0)?;
    let mut total = 0;

    let (stat_tx, stat_rx) = cbc::unbounded::<StatusUpdate>();
    let (file_tx, file_rx) = cbc::unbounded::<CopyOp>();
    let stat_q = StatSender::new(stat_tx, &opts);

    // Start (single) dispatch worker
    let q_opts = opts.clone();
    let dispatcher = thread::spawn(|| dispatch_worker(file_rx, stat_q, q_opts));

    for source in sources {
        let sourcedir = source
            .components()
            .last()
            .ok_or(XcpError::InvalidSource("Failed to find source directory name."))?;

        let target_base = if dest.exists() {
            dest.join(sourcedir)
        } else {
            dest.to_path_buf()
        };
        debug!("Target base is {:?}", target_base);

        let gitignore = parse_ignore(&source, &opts)?;

        for entry in WalkDir::new(&source)
            .into_iter()
            .filter_entry(|e| ignore_filter(e, &gitignore))
        {
            debug!("Got tree entry {:?}", entry);
            let e = entry?;
            let from = e.into_path();
            let meta = from.symlink_metadata()?;
            let path = from.strip_prefix(&source)?;
            let target = if !empty(path) {
                target_base.join(path)
            } else {
                target_base.clone()
            };

            if opts.no_clobber && target.exists() {
                return Err(XcpError::DestinationExists(
                    "Destination file exists and --no-clobber is set.",
                    target,
                )
                .into());
            }

            match FileType::from(meta.file_type()) {
                FileType::File => {
                    debug!("Start copy operation {:?} to {:?}", from, target);
                    file_tx.send(CopyOp {
                        from,
                        target,
                    })?;
                    total += meta.len();
                }

                FileType::Symlink => {
                    let lfile = read_link(from)?;
                    debug!("Creating symlink from {:?} to {:?}", lfile, target);
                    let _r = symlink(&lfile, &target);
                }

                FileType::Dir => {
                    debug!("Creating target directory {:?}", target);
                    create_dir_all(&target)?;
                }

                FileType::Socket | FileType::Char | FileType::Fifo => {
                    debug!("Copy special file {:?} to {:?}", from, target);
                    copy_node(&from, &target)?;
                }

                FileType::Other => {
                    error!("Unknown filetype found; this should never happen!");
                    return Err(XcpError::UnknownFileType(target).into());
                }
            };
        }
    }

    drop(file_tx);
    pb.set_size(total);
    for stat in stat_rx {
        match stat {
            StatusUpdate::Copied(v) => pb.inc(v),
            StatusUpdate::Size(v) => pb.inc_size(v),
            StatusUpdate::Error(e) => {
                // FIXME: Optional continue?
                error!("Received error: {}", e);
                return Err(e.into());
            }
        }
    }
    pb.end();

    // Join the dispatch thread to ensure we pickup any errors not on
    // the queue. Ideally this shouldn't happen though.
    dispatcher.join()
        .map_err(|_| XcpError::CopyError("Error dispatching copy operation".to_string()))??;

    Ok(())
}
