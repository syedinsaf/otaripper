<!-- markdownlint-configure-file {
  "MD033": false,
  "MD041": false
} -->

<div align="center">
# otaripper

**`otaripper` helps you extract partitions from Android OTA files.** <br />
Partitions can be individually flashed to your device using `fastboot flash`.

Compared to other tools, `otaripper` is significantly faster and handles file
verification - no fear of a bad OTA file bricking your device.

![Demo][demo]

</div>

## Features

|                              | [syedinsaf/otaripper] | [ssut/payload-dumper-go] | [vm03/payload_dumper]                     |
| ---------------------------- | --------------------- | ------------------------ | ----------------------------------------- |
| Input file verification      | ✔                     | ✔                        |                                           |
| Output file verification     | ✔                     |                          |                                           |
| Extract selective partitions | ✔                     | ✔                        | ✔                                         |
| Parallelized extraction      | ✔                     | ✔                        |                                           |
| Runs directly on .zip files  | ✔                     | ✔                        |                                           |
| Incremental OTA support      |                       |                          | [Partial][payload_dumper-incremental-ota] |



## Installation

### macOS / Linux

Install a pre-built binary:

```sh
curl -sS https://raw.githubusercontent.com/syedinsaf/otaripper/main/install.sh | bash
```

### Windows

Download the pre-built binary from the [Releases] page. Extract it and run the `otaripper.exe` file.

## Usage

Run the following command in your terminal:

```sh
# Run directly on .zip file.
otaripper ota.zip (on Windows)
./otaripper ota.zip (on Linux)

# Run on payload.bin file.
otaripper payload.bin
./otaripper ota.bin (on Linux)

```
## To extract your desired Partitions add " --partitions" and then your desired ".img"

```sh
# For example, if you want to extract just the boot image, you can do this:
./otaripper  payload.bin --partitions boot

# If you want multiple desired images, you can separate them by a ","
./otaripper  payload.bin --partitions boot,init_boot
```
## Contributors

- [Syed Insaf][syedinsaf]

[syedinsaf]: https://github.com/syedinsaf
[payload_dumper-incremental-ota]: https://github.com/vm03/payload_dumper/issues/53
[releases]: https://github.com/syedinsaf/otaripper/releases
[syedinsaf/otaripper]: https://github.com/syedinsaf/otaripper
[ssut/payload-dumper-go]: https://github.com/ssut/payload-dumper-go
[vm03/payload_dumper]: https://github.com/vm03/payload_dumper
