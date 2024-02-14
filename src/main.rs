use clap::{Parser, ValueEnum};
use fatfs::*;
use log::*;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Directory root to convert to an image
    #[arg(short, long)]
    input_dir: PathBuf,
    /// Partition table to use. Image size may be extended to fit it
    #[arg(value_enum, short, long, default_value = "none")]
    partition_table: PartitionTable,
    /// Filesystem for the image
    #[arg(value_enum, short, long, default_value = "vfat")]
    filesystem: Filesystem,
    /// Output image path
    #[arg(short, long)]
    output_path: PathBuf,
    /// Set partition size. If not set, is estimated automatically
    #[arg(short, long)]
    size: Option<u64>,
    /// Whether image should be bootable
    #[arg(short, long)]
    bootable: bool,
    /// Whether to follow symlinks or skip them
    #[arg(short, long)]
    link_follow: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum PartitionTable {
    #[value(alias("gpt"))]
    Gpt,
    #[value(alias("mbr"))]
    Mbr,
    #[value(alias("none"))]
    None,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum Filesystem {
    #[value(alias("vfat"), alias("fat32"))]
    Vfat,
}

impl Filesystem {
    fn estimate_size(&self, input_dir: &Path, link_follow: bool) -> anyhow::Result<u64> {
        Ok(match self {
            Self::Vfat => {
                // Estimate size for fat32 images. They will be sufficient for smaller images.
                let mut files = 0;
                let mut number_of_fats = 3;
                let mut dir_entries = 1u64;

                let dir_entry_count = (FAT_BYTES_PER_CLUSTER / 32) as u64;
                let dir_entry_align = dir_entry_count - 1;

                walk_dir(
                    input_dir,
                    input_dir,
                    link_follow,
                    dir_entries,
                    &mut |cur_path, _, dir_entries, _| {
                        *dir_entries += 1;
                        // Long file name
                        let file_len = cur_path.file_name().map(|f| f.len() as u64).unwrap_or(0);
                        let lfn_entries = (file_len + 12) / 13;
                        *dir_entries += lfn_entries;

                        // Including . and .. entries
                        Ok(3)
                    },
                    &mut |cur_path, _, dir_entries, metadata| {
                        files += 1;
                        *dir_entries += 1;
                        // Number of FAT
                        number_of_fats +=
                            (metadata.len() + FAT_ALIGN as u64) / FAT_BYTES_PER_CLUSTER as u64;
                        // Long file name
                        let file_len = cur_path.file_name().map(|f| f.len() as u64).unwrap_or(0);
                        let lfn_entries = (file_len + 12) / 13;
                        *dir_entries += lfn_entries;
                        Ok(())
                    },
                    &mut |_, counted_entries| {
                        // Final dir entry alignment
                        dir_entries = (dir_entries + dir_entry_align) & !dir_entry_align;
                        dir_entries += (counted_entries + dir_entry_align) & !dir_entry_align;
                        Ok(())
                    },
                )?;

                // fatrs implementation reserves 8 sectors
                let reserved_sectors = FAT_BYTES_PER_SECTOR as u64 * 8;

                let size = number_of_fats * FAT_BYTES_PER_CLUSTER as u64;

                number_of_fats += 3;

                debug!(
                    r"
    size: {size:x}
    number_of_fats: {number_of_fats:x}
    dir_entries: {dir_entries}"
                );

                size + number_of_fats * 4 * 2 + reserved_sectors + dir_entries * 32
            }
        })
    }
}

const FAT_BYTES_PER_CLUSTER: usize = 512;
const FAT_ALIGN: usize = FAT_BYTES_PER_CLUSTER - 1;
const FAT_BYTES_PER_SECTOR: usize = 512;

fn walk_dir<T>(
    root: &Path,
    cur_path: &Path,
    link_follow: bool,
    mut cur_entry: T,
    dir_cb: &mut impl FnMut(&Path, &Path, &mut T, &Metadata) -> io::Result<T>,
    file_cb: &mut impl FnMut(&Path, &Path, &mut T, &Metadata) -> io::Result<()>,
    close_cb: &mut impl FnMut(&Path, T) -> io::Result<()>,
) -> io::Result<()> {
    for entry in fs::read_dir(cur_path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let path = entry.path();
        if let Ok(short_path) = path.strip_prefix(root) {
            if metadata.is_dir() {
                let new_entry = dir_cb(&path, &short_path, &mut cur_entry, &metadata)?;
                std::mem::drop(metadata);
                walk_dir(
                    root,
                    &path,
                    link_follow,
                    new_entry,
                    dir_cb,
                    file_cb,
                    close_cb,
                )
                .unwrap();
            } else if link_follow || !metadata.is_symlink() {
                file_cb(&path, &short_path, &mut cur_entry, &metadata)?;
                std::mem::drop(metadata);
            } else {
                warn!("Skipping symlink - {}", short_path.display());
            }
        } else {
            error!("walk_dir: {path:?}");
        }
    }

    close_cb(cur_path, cur_entry)?;

    Ok(())
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Args::parse();

    let partition_size = if let Some(size) = args.size {
        size
    } else {
        args.filesystem
            .estimate_size(&args.input_dir, args.link_follow)?
    };

    debug!("Partition size: {partition_size:x}");

    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(args.output_path)?;

    let fat_slice = match args.partition_table {
        PartitionTable::None => {
            file.set_len(partition_size)?;

            Box::new(file) as Box<dyn ReadWriteSeek>
        }
        PartitionTable::Mbr => {
            // Align to 512 byte sector
            let partition_size = (partition_size + 0x1ff) & !0x1ff;

            file.set_len(partition_size + 0x200)?;

            let mut mbr = mbrman::MBR::new_from(&mut file, 0x200, (!0u32).to_ne_bytes())?;
            mbr.align = 1;

            let sectors = (partition_size / 0x200) as u32;

            // This should never panic
            let starting_lba = mbr.find_optimal_place(sectors).unwrap();

            mbr[1] = mbrman::MBRPartitionEntry {
                boot: if args.bootable {
                    mbrman::BOOT_ACTIVE
                } else {
                    mbrman::BOOT_INACTIVE
                },
                first_chs: mbrman::CHS::empty(),
                sys: 0xef,
                last_chs: mbrman::CHS::empty(),
                starting_lba,
                sectors,
            };

            mbr.write_into(&mut file)?;

            let part_start = starting_lba as u64 * 0x200;
            let part_len = sectors as u64 * 0x200;

            debug!("part_start: {part_start:x} part_len: {part_len:x}");

            let fat_slice = fscommon::StreamSlice::new(file, part_start, part_start + part_len)?;

            Box::new(fat_slice)
        }
        PartitionTable::Gpt => {
            let total_size = partition_size + 0x20000;

            debug!("Total size: {total_size:x}");

            file.set_len(total_size)?;

            let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
                u32::try_from((total_size / 512) - 1).unwrap_or(0xFF_FF_FF_FF),
            );
            mbr.overwrite_lba0(&mut file).expect("failed to write MBR");

            let mut gdisk = gpt::GptConfig::default()
                .initialized(false)
                .writable(true)
                .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
                .create_from_device(Box::new(file), None)?;

            gdisk.update_partitions(
                std::collections::BTreeMap::<u32, gpt::partition::Partition>::new(),
            )?;

            let part =
                gdisk.add_partition("EFI", partition_size, gpt::partition_types::EFI, 0, None)?;

            let part = gdisk.partitions().get(&part).unwrap();

            let lb_size = gdisk.logical_block_size();
            let part_start = part.bytes_start(*lb_size).unwrap();
            let part_len = part.bytes_len(*lb_size).unwrap();

            let file = gdisk.write().unwrap();

            debug!("part_start: {part_start:x} part_len: {part_len:x}");

            let fat_slice = fscommon::StreamSlice::new(file, part_start, part_start + part_len)?;

            Box::new(fat_slice)
        }
    };

    let mut buf_stream = fscommon::BufStream::new(fat_slice);

    format_volume(
        &mut buf_stream,
        FormatVolumeOptions::new().bytes_per_cluster(FAT_BYTES_PER_CLUSTER as u32),
    )?;

    let fs = FileSystem::new(buf_stream, FsOptions::new())?;

    let root_dir = fs.root_dir();

    let mut cnt = 0;

    walk_dir(
        &args.input_dir,
        &args.input_dir,
        args.link_follow,
        root_dir,
        &mut |_, short_path, parent_dir, _| {
            let name = short_path.file_name().unwrap().to_str().unwrap();
            info!("DIR: {name}");
            Ok(parent_dir.create_dir(name)?)
        },
        &mut |path, short_path, parent_dir: &mut Dir<_>, _| {
            let name = short_path.file_name().unwrap().to_str().unwrap();
            cnt += 1;
            info!("FILE {cnt}: {name}");
            let mut orig_file = File::open(path)?;
            let mut file = parent_dir.create_file(name)?;
            std::io::copy(&mut orig_file, &mut file)?;
            Ok(())
        },
        &mut |_, _| Ok(()),
    )?;

    Ok(())
}
