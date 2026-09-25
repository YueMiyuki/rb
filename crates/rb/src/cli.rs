use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "rb",
    version,
    about = "A cargo-compatible Rust builder with a global content-addressed store, critical-path scheduling and zero-setup cross compilation",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    #[arg(short, long, global = true)]
    pub quiet: bool,
    #[arg(long, global = true, value_name = "WHEN")]
    pub color: Option<String>,
    #[arg(long, global = true, value_name = "KEY=VALUE")]
    pub config: Vec<String>,
    #[arg(short = 'Z', global = true, value_name = "FLAG")]
    pub unstable: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    Search(SearchArgs),
    Package(PackageArgs),
    New(NewArgs),
    Init(InitArgs),
    #[command(visible_alias = "b")]
    Build(BuildArgs),
    Rustc(RustcArgs),
    Rustdoc(RustcArgs),
    #[command(visible_alias = "c")]
    Check(BuildArgs),
    #[command(visible_alias = "r")]
    Run(RunArgs),
    #[command(visible_alias = "t")]
    Test(TestArgs),
    Bench(TestArgs),
    #[command(visible_alias = "d")]
    Doc(DocArgs),
    Clean(CleanArgs),
    Store {
        #[command(subcommand)]
        cmd: StoreCmd,
    },
    Target {
        #[command(subcommand)]
        cmd: TargetCmd,
    },
    Doctor(DoctorArgs),
    Update(UpdateArgs),
    Add(AddArgs),
    #[command(visible_alias = "rm")]
    Remove(RemoveArgs),
    Install(InstallArgs),
    Uninstall(UninstallArgs),
    Metadata(MetadataArgs),
    Fetch(FetchArgs),
    Pkgid(PkgidArgs),
    Tree(TreeArgs),
    Vendor(VendorArgs),
    ReadManifest(FetchArgs),
    Version,
    GenerateLockfile(FetchArgs),
    LocateProject(LocateArgs),
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    #[arg(required = true, value_name = "QUERY")]
    pub query: Vec<String>,
    #[arg(long, default_value_t = 10)]
    pub limit: u32,
    #[arg(long)]
    pub offline: bool,
}

#[derive(Args, Debug)]
pub struct PackageArgs {
    #[arg(short, long)]
    pub list: bool,
    #[arg(long)]
    pub no_verify: bool,
    #[arg(long)]
    pub allow_dirty: bool,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub frozen: bool,
}

#[derive(Args, Debug)]
pub struct NewArgs {
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
    #[arg(long)]
    pub bin: bool,
    #[arg(long)]
    pub lib: bool,
    #[arg(long)]
    pub edition: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub vcs: Option<String>,
}

#[derive(Args, Debug)]
pub struct InitArgs {
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
    #[arg(long)]
    pub bin: bool,
    #[arg(long)]
    pub lib: bool,
    #[arg(long)]
    pub edition: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub vcs: Option<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct RustcArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    #[arg(last = true)]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct RunArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    #[arg(last = true)]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct TestArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    #[arg(value_name = "TESTNAME")]
    pub filter: Option<String>,
    #[arg(long)]
    pub no_run: bool,
    #[arg(long)]
    pub no_fail_fast: bool,
    #[arg(long)]
    pub doc: bool,
    #[arg(last = true)]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct DocArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    #[arg(long)]
    pub no_deps: bool,
    #[arg(long)]
    pub open: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct BuildArgs {
    #[arg(short, long = "package", value_name = "SPEC")]
    pub package: Vec<String>,
    #[arg(long, visible_alias = "all")]
    pub workspace: bool,
    #[arg(long, value_name = "SPEC")]
    pub exclude: Vec<String>,
    #[arg(long)]
    pub lib: bool,
    #[arg(long, value_name = "NAME")]
    pub bin: Vec<String>,
    #[arg(long)]
    pub bins: bool,
    #[arg(long, value_name = "NAME")]
    pub example: Vec<String>,
    #[arg(long)]
    pub examples: bool,
    #[arg(long, value_name = "NAME")]
    pub test: Vec<String>,
    #[arg(long)]
    pub tests: bool,
    #[arg(long, value_name = "NAME")]
    pub bench: Vec<String>,
    #[arg(long)]
    pub benches: bool,
    #[arg(long)]
    pub all_targets: bool,
    #[arg(short, long)]
    pub release: bool,
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    #[arg(long, value_name = "TRIPLE")]
    pub target: Vec<String>,
    #[arg(short, long, value_name = "N")]
    pub jobs: Option<usize>,
    #[arg(long)]
    pub keep_going: bool,
    #[arg(short = 'F', long, value_name = "FEATURES")]
    pub features: Vec<String>,
    #[arg(long)]
    pub all_features: bool,
    #[arg(long)]
    pub no_default_features: bool,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub frozen: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long, value_name = "DIRECTORY")]
    pub target_dir: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub timings: bool,
    #[arg(long)]
    pub unit_graph: bool,
    #[arg(long)]
    pub no_store: bool,
    #[arg(long, value_name = "MODE")]
    pub link_mode: Option<String>,
    #[arg(long)]
    pub no_cache_build_scripts: bool,
    #[arg(long)]
    pub no_provision: bool,
    #[arg(long, value_name = "FMT")]
    pub message_format: Option<String>,
    #[arg(long)]
    pub ignore_rust_version: bool,
    #[arg(long)]
    pub future_incompat_report: bool,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    #[arg(value_name = "SPEC")]
    pub specs: Vec<String>,
    #[arg(short, long = "package", value_name = "SPEC")]
    pub package: Vec<String>,
    #[arg(long)]
    pub precise: Option<String>,
    #[arg(long)]
    pub recursive: bool,
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    #[arg(short = 'w', long)]
    pub workspace: bool,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub frozen: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub ignore_rust_version: bool,
}

#[derive(Args, Debug)]
pub struct AddArgs {
    #[arg(value_name = "DEP")]
    pub deps: Vec<String>,
    #[arg(long)]
    pub path: Option<PathBuf>,
    #[arg(long)]
    pub git: Option<String>,
    #[arg(long)]
    pub branch: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub rev: Option<String>,
    #[arg(short = 'F', long)]
    pub features: Vec<String>,
    #[arg(long)]
    pub no_default_features: bool,
    #[arg(long)]
    pub optional: bool,
    #[arg(long)]
    pub dev: bool,
    #[arg(long)]
    pub build: bool,
    #[arg(long)]
    pub rename: Option<String>,
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    #[arg(short, long = "package", value_name = "SPEC")]
    pub package: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub frozen: bool,
    #[arg(long)]
    pub ignore_rust_version: bool,
}

#[derive(Args, Debug)]
pub struct RemoveArgs {
    #[arg(value_name = "DEP", required = true)]
    pub deps: Vec<String>,
    #[arg(long)]
    pub dev: bool,
    #[arg(long)]
    pub build: bool,
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    #[arg(short, long = "package", value_name = "SPEC")]
    pub package: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub frozen: bool,
}

#[derive(Args, Debug)]
pub struct UninstallArgs {
    #[arg(value_name = "CRATE")]
    pub name: String,
    #[arg(long)]
    pub root: Option<PathBuf>,
}

#[derive(Args, Debug)]
#[command(disable_version_flag = true)]
pub struct InstallArgs {
    #[arg(value_name = "CRATE")]
    pub name: Option<String>,
    #[arg(long)]
    pub version: Option<String>,
    #[arg(long)]
    pub path: Option<PathBuf>,
    #[arg(long)]
    pub git: Option<String>,
    #[arg(long)]
    pub branch: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub rev: Option<String>,
    #[arg(long, value_name = "NAME")]
    pub bin: Vec<String>,
    #[arg(short, long)]
    pub force: bool,
    #[arg(long)]
    pub root: Option<PathBuf>,
    #[arg(long)]
    pub list: bool,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub frozen: bool,
    #[arg(short, long)]
    pub jobs: Option<usize>,
}

#[derive(Args, Debug)]
pub struct MetadataArgs {
    #[arg(long, value_name = "VERSION")]
    pub format_version: u32,
    #[arg(long)]
    pub no_deps: bool,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub frozen: bool,
}

#[derive(Args, Debug)]
pub struct TreeArgs {
    #[arg(short, long = "package", value_name = "SPEC")]
    pub package: Option<String>,
    #[arg(long)]
    pub depth: Option<usize>,
    #[arg(long, default_value = "normal")]
    pub edges: String,
    #[arg(short = 'i', long, value_name = "SPEC")]
    pub invert: Option<String>,
    #[arg(long, default_value = "utf8")]
    pub charset: String,
    #[arg(long, default_value = "indent")]
    pub prefix: String,
    #[arg(long, default_value = "{p}")]
    pub format: String,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub frozen: bool,
}

#[derive(Args, Debug)]
pub struct PkgidArgs {
    #[arg(value_name = "SPEC")]
    pub spec: Option<String>,
    #[arg(short, long = "package", value_name = "SPEC")]
    pub package: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub frozen: bool,
}

#[derive(Args, Debug)]
pub struct VendorArgs {
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
    #[arg(long)]
    pub no_delete: bool,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub frozen: bool,
}

#[derive(Args, Debug)]
pub struct LocateArgs {
    #[arg(long)]
    pub workspace: bool,
    #[arg(long, default_value = "json")]
    pub message_format: String,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct FetchArgs {
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    #[arg(long)]
    pub locked: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub frozen: bool,
}

#[derive(Args, Debug)]
pub struct CleanArgs {
    #[arg(long, value_name = "DIRECTORY")]
    pub target_dir: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum StoreCmd {
    Status,
    Path,
    Gc {
        #[arg(long, value_name = "SIZE")]
        max_size: Option<String>,
        #[arg(long, value_name = "DAYS")]
        max_age_days: Option<u64>,
        #[arg(long)]
        dry_run: bool,
    },
    Verify {
        #[arg(long)]
        repair: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum TargetCmd {
    Add {
        #[arg(required = true, value_name = "TRIPLE")]
        targets: Vec<String>,
        #[arg(long)]
        accept_license: bool,
    },
    List,
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    #[arg(long)]
    pub smoke: bool,
}
