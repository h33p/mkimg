# mkimg

Simple tool to create raw disk images.

## Install

```
cargo install mkimg
```

## Usage

Create a simple fat32 image without additional partition table:

```
$ mkimg -i directory -o image.raw
```

Create a fat32 image with GPT partition table:

```
$ mkimg -i directory -o image.raw -p gpt
```

See all options:

```
$ mkimg -h
Simple tool to create raw disk images

Usage: mkimg [OPTIONS] --input-dir <INPUT_DIR> --output-path <OUTPUT_PATH>

Options:
  -i, --input-dir <INPUT_DIR>
          Directory root to convert to an image
  -p, --partition-table <PARTITION_TABLE>
          Partition table to use. Image size may be extended to fit it [default: none] [possible values: gpt, mbr, none]
  -f, --filesystem <FILESYSTEM>
          Filesystem for the image [default: fat32] [possible values: fat32]
  -o, --output-path <OUTPUT_PATH>
          Output image path
  -s, --size <SIZE>
          Set partition size. If not set, is estimated automatically
  -b, --bootable
          Whether image should be bootable
  -h, --help
          Print help information
  -V, --version
          Print version information
```
