pub mod arbscan;
pub mod extractor;
pub mod simd;

use crate::cmd::extractor::Extractor;
use anyhow::Result;
use clap::{Parser, ValueHint};
use std::path::PathBuf;

#[derive(Debug, clap::Subcommand)]
pub enum SubCmd {
    /// Remove extracted_* folders
    #[clap(aliases = &["c"])]
    Clean {
        /// Clean extracted_* folders inside this directory
        #[clap(
            short = 'o',
            long = "output-dir",
            value_name = "PATH",
            value_hint = clap::ValueHint::DirPath
        )]
        output_dir: Option<PathBuf>,
    },
    /// Extract OEM Anti-Rollback (ARB) metadata from Qualcomm bootloader images
    #[clap(aliases = &["arb"])]
    Arbscan {
        /// Disable interactive prompt for JSON output
        #[clap(long)]
        no_json: bool,

        /// Path to the bootloader image (e.g., xbl_config.img)
        #[clap(value_hint = clap::ValueHint::FilePath, value_name = "PATH")]
        image: PathBuf,
    },
}

#[derive(Debug, Parser)]
#[clap(
    about,
    author,
    help_template = FRIENDLY_HELP,
    propagate_version = true,
    version = env!("CARGO_PKG_VERSION"),
)]
pub struct Cmd {
    #[clap(subcommand)]
    pub(super) subcmd: Option<SubCmd>,
    /// List partitions instead of extracting them
    #[clap(
        conflicts_with = "threads",
        conflicts_with = "output_dir",
        conflicts_with = "partitions",
        conflicts_with = "no_verify",
        long,
        short
    )]
    pub(super) list: bool,

    /// Number of threads to use during extraction
    #[clap(long, short, value_name = "NUMBER")]
    pub(super) threads: Option<usize>,

    /// Set output directory
    #[clap(long, short, value_hint = ValueHint::DirPath, value_name = "PATH")]
    pub(super) output_dir: Option<PathBuf>,

    /// Dump only selected partitions (comma-separated)
    #[clap(short = 'p', long, value_delimiter = ',', value_name = "PARTITIONS")]
    pub(super) partitions: Vec<String>,

    /// Skip file verification (dangerous!)
    #[clap(long, conflicts_with = "strict")]
    pub(super) no_verify: bool,

    /// Require cryptographic hashes and enforce verification; fails if any required hash is missing
    #[clap(
        long,
        help = "Require manifest hashes for partitions and operations; enforce verification and fail if any required hash is missing."
    )]
    pub(super) strict: bool,

    /// Compute and print SHA-256 of each extracted partition image
    #[clap(
        long,
        help = "Compute and print the SHA-256 of each extracted partition image. If the manifest lacks a hash, this may add one linear pass over the image."
    )]
    pub(super) print_hash: bool,

    /// Run lightweight sanity checks on output images (e.g., detect all-zero images)
    #[clap(
        long,
        help = "Run quick sanity checks on output images and fail on obviously invalid content (e.g., all zeros)."
    )]
    pub(super) sanity: bool,

    /// Print per-partition and total timing/throughput statistics after extraction
    #[clap(
        long,
        help = "Print per-partition and total timing/throughput statistics after extraction."
    )]
    pub(super) stats: bool,

    /// Don't automatically open the extracted folder after completion
    #[clap(
        long,
        short = 'n',
        help = "Don't automatically open the extracted folder after completion."
    )]
    pub(super) no_open: bool,

    /// Positional argument for the payload file
    #[clap(value_hint = ValueHint::FilePath)]
    #[clap(index = 1, value_name = "PATH")]
    pub(super) positional_payload: Option<PathBuf>,
}

impl Cmd {
    pub fn run(&self) -> Result<()> {
        Extractor { cmd: self }.run()
    }
}

const FRIENDLY_HELP: &str = color_print::cstr!(
    "\
{before-help}<bold><underline>{name} {version}</underline></bold>
{about}

<bold>QUICK START</bold>
  • Drag & drop an OTA .zip or payload.bin onto the executable.
  • Or run via command line: <cyan>otaripper update.zip</cyan>

<bold>COMMON TASKS</bold>
  • <bold>List</bold> partitions:                            otaripper -l update.zip
  • <bold>Extract everything</bold>:                         otaripper update.zip
  • <bold>Extract specific</bold>:                           otaripper update.zip -p boot,init_boot,vendor_boot
  • <bold>Disable auto-open folder after extraction: </bold> otaripper update.zip -n
  • <bold>Scan bootloader for ARB metadata: </bold>          otaripper arbscan xbl_config.img

<bold>CLEANUP</bold>
    • <bold>Remove extracted folders</bold>:                 otaripper clean
    • <bold>Clean in specific directory</bold>:              otaripper clean -o /path/to/dir

<bold>SAFETY & INTEGRITY</bold>
  • SHA-256 verification is <green>enabled by default</green>.
  • Partial files are <red>automatically deleted</red> on failure.
  • Use <yellow>--strict</yellow> to require manifest hashes and enforce verification.
  • Skip verification (not recommended): <yellow>--no-verify</yellow>

<bold>QUALITY OF LIFE</bold>
  • Automatically opens extracted folder after success.
  • Disable opening folder: <yellow>-n</yellow> or <yellow>--no-open</yellow>

{usage-heading}
  {usage}

<bold>OPTIONS</bold>
{all-args}

<bold>PROJECT</bold>: <blue>https://github.com/syedinsaf/otaripper</blue>
{after-help}"
);
