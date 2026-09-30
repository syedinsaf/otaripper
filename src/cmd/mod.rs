pub mod arbscan;
pub mod extractor;
pub mod metadata;
pub mod simd;

use crate::cmd::extractor::Extractor;
use anyhow::Result;
use clap::{Parser, ValueHint};
use std::path::PathBuf;

#[derive(Debug, clap::Subcommand)]
pub enum SubCmd {
    /// Remove extracted_* folders
    #[clap(alias = "c")]
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
    #[clap(
        alias = "arb",
        override_usage = "otaripper arbscan [OPTIONS] <PATH>\n\n  Note: The 'arb' subcommand only supports the '-n' / '--no-json' flag. Global extraction flags like -l, --strict, or --sanity are not applicable here."
    )]
    Arbscan {
        /// Disable interactive prompt for JSON output
        #[clap(short = 'n', long)]
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
    #[clap(index = 1, value_name = "PATH_OR_URL")]
    pub(super) positional_payload: Option<PathBuf>,

    /// Internal flag to suppress output
    #[clap(skip)]
    pub(super) quiet: bool,
}

impl Cmd {
    pub fn run(&self) -> Result<()> {
        Extractor { cmd: self }.run()
    }
}

const FRIENDLY_HELP: &str = color_print::cstr!(
    "\
{before-help}<bold>
<rgb(255,0,0)>           █████                         ███                                        </>
<rgb(255,40,0)>          ░░███                         ░░░                                         </>
<rgb(255,80,0)>  ██████  ███████    ██████   ████████  ████  ████████  ████████   ██████  ████████ </>
<rgb(255,120,0)> ███░░███░░░███░    ░░░░░███ ░░███░░███░░███ ░░███░░███░░███░░███ ███░░███░░███░░███</>
<rgb(255,150,0)>░███ ░███  ░███      ███████  ░███ ░░░  ░███  ░███ ░███ ░███ ░███░███████  ░███ ░░░ </>
<rgb(255,180,0)>░███ ░███  ░███ ███ ███░░███  ░███      ░███  ░███ ░███ ░███ ░███░███░░░   ░███     </>
<rgb(255,200,0)>░░██████   ░░█████ ░░████████ █████     █████ ░███████  ░███████ ░░██████  █████    </>
<rgb(255,220,0)> ░░░░░░     ░░░░░   ░░░░░░░░ ░░░░░     ░░░░░  ░███░░░   ░███░░░   ░░░░░░  ░░░░░     </>
<rgb(255,235,0)>                                              ░███      ░███                        </>
<rgb(255,245,0)>                                              █████     █████                       </>
<rgb(255,255,0)>                                             ░░░░░     ░░░░░                        </></bold>

  <bold><bright-cyan>v{version}</bright-cyan></bold> <dim>│</dim> {about}

<bold><bright-cyan>▸ QUICK START</bright-cyan></bold>
  <cyan>•</cyan> Drag & drop an OTA <dim>.zip</dim> or <dim>payload.bin</dim> onto the executable.
  <cyan>•</cyan> Extract from local file:                 <cyan>otaripper</cyan> <bright-white>update.zip</bright-white>
  <cyan>•</cyan> Stream directly from URL:                <cyan>otaripper</cyan> <bright-white>https://example.com/ota.zip</bright-white>

<bold><bright-cyan>▸ COMMON TASKS</bright-cyan></bold>
  <dim><i>(Tip: You can replace 'update.zip' with an HTTP URL in any command!)</i></dim>
  <cyan>•</cyan> List remote/local partitions:             <cyan>otaripper</cyan> <yellow>-l</yellow> <bright-white>update.zip</bright-white>
  <cyan>•</cyan> Extract all partitions:                   <cyan>otaripper</cyan> <bright-white>update.zip</bright-white>
  <cyan>•</cyan> Extract specific partitions:              <cyan>otaripper</cyan> <bright-white>update.zip</bright-white> <yellow>-p</yellow> <bright-white>boot,init_boot,vendor_boot</bright-white>
  <cyan>•</cyan> Disable auto-open after extraction:       <cyan>otaripper</cyan> <bright-white>update.zip</bright-white> <yellow>-n</yellow>
  <cyan>•</cyan> Scan bootloader for ARB metadata:         <cyan>otaripper</cyan> <yellow>arbscan</yellow> <bright-white>xbl_config.img</bright-white>

<bold><bright-cyan>▸ CLEANUP</bright-cyan></bold>
  <cyan>•</cyan> Remove extracted folders:                 <cyan>otaripper</cyan> <yellow>clean</yellow>
  <cyan>•</cyan> Clean in specific directory:              <cyan>otaripper</cyan> <yellow>clean -o</yellow> <bright-white>/path/to/dir</bright-white>

<bold><bright-cyan>▸ SAFETY & INTEGRITY</bright-cyan></bold>
  <cyan>•</cyan> SHA-256 verification is <green>enabled by default</green>.
  <cyan>•</cyan> Partial files are <red>automatically deleted</red> on failure.
  <cyan>•</cyan> Require manifest hashes & strict check:   <yellow>--strict</yellow>
  <cyan>•</cyan> Skip verification (not recommended):     <yellow>--no-verify</yellow>

<bold><bright-cyan>▸ QUALITY OF LIFE</bright-cyan></bold>
  <cyan>•</cyan> Automatically opens extracted folder after success.
  <cyan>•</cyan> Disable opening folder:                   <yellow>-n</yellow> or <yellow>--no-open</yellow>

<bold><bright-cyan>▸ USAGE</bright-cyan></bold>
  {usage}

<bold><bright-cyan>▸ OPTIONS & COMMANDS</bright-cyan></bold>
{all-args}

<bold><bright-cyan>▸ PROJECT REPO</bright-cyan></bold>  <cyan>→</cyan>  <blue><underline>https://github.com/syedinsaf/otaripper</underline></blue>
{after-help}
"
);
