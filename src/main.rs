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
    #[arg(value_enum, short, long, default_value = "fat32")]
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
    #[value(alias("fat32"))]
    Fat32,
}

fn walk_dir<T>(
    root: &Path,
    cur_path: &Path,
    cur_entry: &T,
    dir_cb: &mut impl FnMut(&Path, &Path, &T, &Metadata) -> io::Result<T>,
    file_cb: &mut impl FnMut(&Path, &Path, &T, &Metadata) -> io::Result<()>,
) -> io::Result<()> {
    for entry in fs::read_dir(cur_path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let path = entry.path();
        if let Ok(short_path) = path.strip_prefix(root) {
            if metadata.is_dir() {
                let cur_entry = dir_cb(&path, &short_path, cur_entry, &metadata)?;
                std::mem::drop(metadata);
                walk_dir(root, &path, &cur_entry, dir_cb, file_cb).unwrap();
            } else {
                file_cb(&path, &short_path, cur_entry, &metadata)?;
                std::mem::drop(metadata);
            }
        } else {
            error!("walk_dir: {path:?}");
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Args::parse();

    let partition_size = if let Some(size) = args.size {
        size
    } else {
        let mut dirs = 0;
        let mut size = 0;
        let mut files = 0;

        walk_dir(
            &args.input_dir,
            &args.input_dir,
            &(),
            &mut |_, _, _, _| {
                dirs += 1;
                Ok(())
            },
            &mut |_, _, _, metadata| {
                files += 1;
                size += metadata.len();
                Ok(())
            },
        )?;

        debug!("size: {size:x} files: {files} dirs: {dirs}");

        size + 0x10000 + (files + dirs) * 512
    };

    debug!("Partition size: {partition_size:x}");

    let fat_slice = match args.partition_table {
        PartitionTable::None => {
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(args.output_path)?;

            file.set_len(partition_size)?;

            Box::new(file) as Box<dyn ReadWriteSeek>
        }
        PartitionTable::Mbr => {
            let mut file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(args.output_path)?;

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

            let mut file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(args.output_path)?;

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
        FormatVolumeOptions::new().fat_type(FatType::Fat32),
    )?;

    let fs = FileSystem::new(buf_stream, FsOptions::new())?;

    let root_dir = fs.root_dir();

    walk_dir(
        &args.input_dir,
        &args.input_dir,
        &root_dir,
        &mut |_, short_path, parent_dir, _| {
            let name = short_path.file_name().unwrap().to_str().unwrap();
            info!("DIR: {name}");
            Ok(parent_dir.create_dir(name)?)
        },
        &mut |path, short_path, parent_dir, _| {
            let name = short_path.file_name().unwrap().to_str().unwrap();
            info!("FILE: {name}");
            let mut orig_file = File::open(path)?;
            let mut file = parent_dir.create_file(name)?;
            std::io::copy(&mut orig_file, &mut file)?;
            Ok(())
        },
    )?;

    Ok(())
}
