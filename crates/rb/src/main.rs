mod cli;
mod ui;

use anyhow::Result;
use clap::Parser;
use cli::{AddArgs, BuildArgs, Cli, Cmd, InstallArgs, MetadataArgs, RemoveArgs, StoreCmd, TargetCmd, TestArgs, UpdateArgs};
use rb_core::config::RbConfig;
use rb_core::shell::Shell;
use rb_core::unit::{Command, TargetFilter};
use rb_core::{AlreadyReported, BuildOptions};

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // Shims call back into rb for every C compile, link, and manifest merge.
    type Shim = fn(&[String]) -> Result<i32>;
    let shim: Option<Shim> = match argv.get(1).map(String::as_str) {
        Some("__zig") => Some(rb_toolchain::zig::run_wrapper),
        Some("__mt") => Some(rb_toolchain::mt::run),
        Some("__compact") => Some(|_| rb_core::build::compact(&RbConfig::load()?).map(|_| 0)),
        _ => None,
    };
    if let Some(run_shim) = shim {
        match run_shim(&argv[2..]) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("rb {}: {e:#}", argv[1]);
                std::process::exit(1);
            }
        }
    }
    let cli = Cli::parse();
    let shell = Shell::new(cli.verbose, cli.quiet, cli.color.as_deref());
    if let Err(e) = run(cli, &shell) {
        if e.downcast_ref::<AlreadyReported>().is_none() {
            shell.error(format!("{e:#}"));
        }
        std::process::exit(101);
    }
}

fn build_options(a: BuildArgs, command: Command, unstable: &[String]) -> Result<BuildOptions> {
    let _ = a.future_incompat_report;
    Ok(BuildOptions {
        command,
        filter: TargetFilter {
            lib: a.lib,
            bins: a.bins,
            bin_names: a.bin,
            examples: a.examples,
            example_names: a.example,
            tests: a.tests,
            test_names: a.test,
            benches: a.benches,
            bench_names: a.bench,
            all_targets: a.all_targets,
            doc: false,
        },
        packages: a.package,
        workspace: a.workspace,
        exclude: a.exclude,
        release: a.release,
        profile: a.profile,
        targets: a.target,
        jobs: a.jobs,
        features: a.features,
        all_features: a.all_features,
        no_default_features: a.no_default_features,
        locked: a.locked,
        frozen: a.frozen,
        offline: a.offline,
        target_dir: a.target_dir,
        manifest_path: a.manifest_path,
        keep_going: a.keep_going,
        timings: a.timings,
        unit_graph: a.unit_graph,
        no_store: a.no_store,
        link_mode: a.link_mode.map(|m| m.parse()).transpose().map_err(anyhow::Error::msg)?,
        no_cache_build_scripts: a.no_cache_build_scripts,
        no_provision: a.no_provision,
        ignore_rust_version: a.ignore_rust_version,
        message_format: a
            .message_format
            .as_deref()
            .map(rb_core::shell::MessageFormat::parse)
            .transpose()
            .map_err(anyhow::Error::msg)?
            .unwrap_or_default(),
        unstable: unstable.to_vec(),
        ..Default::default()
    })
}

fn test_options(t: TestArgs, command: Command, unstable: &[String]) -> Result<BuildOptions> {
    let mut o = build_options(t.build, command, unstable)?;
    o.filter.doc = t.doc;
    o.no_run = t.no_run;
    o.no_fail_fast = t.no_fail_fast;
    o.args = t.filter.into_iter().chain(t.args).collect();
    Ok(o)
}

fn run(cli: Cli, shell: &Shell) -> Result<()> {
    let unstable = cli.unstable.clone();
    rb_core::config::set_cli_config(cli.config.clone());
    let cfg = RbConfig::load()?;
    let build = |o: BuildOptions| rb_core::build::run(&o, &cfg, shell);
    match cli.cmd {
        Cmd::Search(a) => {
            let cwd = std::env::current_dir()?;
            let cargo_cfg = rb_core::config::CargoConfig::load(&cwd)?;
            if a.offline || cargo_cfg.offline()? {
                anyhow::bail!("cannot search crates.io while offline");
            }
            let hits = rb_core::registry::search_crates(&a.query.join(" "), a.limit)?;
            if hits.is_empty() {
                println!("no crates matched `{}`", a.query.join(" "));
            }
            let width = hits.iter().map(|h| h.name.len() + h.version.len() + 5).max().unwrap_or(0);
            for hit in hits {
                let head = format!("{} = \"{}\"", hit.name, hit.version);
                if hit.description.is_empty() {
                    println!("{head}");
                } else {
                    println!("{head:<width$} # {}", hit.description);
                }
            }
            Ok(())
        }
        Cmd::Package(a) => rb_core::build::package(
            a.list,
            a.no_verify,
            a.allow_dirty,
            a.manifest_path.as_deref(),
            a.locked || a.frozen,
            a.offline || a.frozen,
            &cfg,
            shell,
        ),
        Cmd::New(a) => {
            if a.bin && a.lib {
                anyhow::bail!("cannot specify both --bin and --lib");
            }
            if !a.path.exists() {
                std::fs::create_dir_all(&a.path)?;
            }
            rb_core::build::scaffold(&a.path, a.lib, a.edition.as_deref(), a.name.as_deref(), a.vcs.as_deref(), shell)
        }
        Cmd::Init(a) => {
            if a.bin && a.lib {
                anyhow::bail!("cannot specify both --bin and --lib");
            }
            let path = a.path.unwrap_or(std::env::current_dir()?);
            rb_core::build::scaffold(&path, a.lib, a.edition.as_deref(), a.name.as_deref(), a.vcs.as_deref(), shell)
        }
        Cmd::Build(a) => build(build_options(a, Command::Build, &unstable)?),
        Cmd::Rustc(a) => {
            let mut o = build_options(a.build, Command::Build, &unstable)?;
            o.extra_rustc = a.args;
            build(o)
        }
        Cmd::Rustdoc(a) => {
            let mut o = build_options(a.build, Command::Doc { deps: false }, &unstable)?;
            o.extra_rustdoc = a.args;
            build(o)
        }
        Cmd::Check(a) => build(build_options(a, Command::Check, &unstable)?),
        Cmd::Run(r) => {
            let mut o = build_options(r.build, Command::Run, &unstable)?;
            o.args = r.args;
            build(o)
        }
        Cmd::Test(t) => build(test_options(t, Command::Test, &unstable)?),
        Cmd::Bench(t) => build(test_options(t, Command::Bench, &unstable)?),
        Cmd::Doc(d) => {
            let mut o = build_options(d.build, Command::Doc { deps: !d.no_deps }, &unstable)?;
            o.open = d.open;
            build(o)
        }
        Cmd::Clean(a) => rb_core::build::clean(a.manifest_path.as_deref(), a.target_dir.as_deref(), shell),
        Cmd::Store { cmd } => store(cmd, &cfg, shell),
        Cmd::Target {
            cmd: TargetCmd::Add { targets, accept_license },
        } => ui::target_add(shell, &cfg, &targets, accept_license),
        Cmd::Target { cmd: TargetCmd::List } => {
            let rustc = rb_toolchain::Rustc::detect(None, &std::env::current_dir()?, &cfg.cache_dir())?;
            ui::print_target_list(&rustc, &cfg.toolchains()?);
            Ok(())
        }
        Cmd::Doctor(args) => ui::doctor(shell, &cfg, args.smoke),
        Cmd::Update(args) => rb_core::build::update(&update_options(args)?, &cfg, shell),
        Cmd::Add(args) => rb_core::build::add(&add_options(args), &cfg, shell),
        Cmd::Remove(args) => rb_core::build::remove(&remove_options(args), &cfg, shell),
        Cmd::Install(args) => rb_core::build::install(&install_options(args), &cfg, shell),
        Cmd::Uninstall(args) => rb_core::build::uninstall(&args.name, args.root.as_deref(), shell),
        Cmd::Metadata(args) => rb_core::build::metadata(&metadata_options(args), &cfg, shell),
        Cmd::Fetch(args) => rb_core::build::fetch(
            args.manifest_path.as_deref(),
            args.locked || args.frozen,
            args.offline || args.frozen,
            &cfg,
            shell,
        ),
        Cmd::ReadManifest(args) => rb_core::build::read_manifest(
            args.manifest_path.as_deref(),
            args.locked || args.frozen,
            args.offline || args.frozen,
            &cfg,
            shell,
        ),
        Cmd::Version => {
            println!("rb {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Cmd::Vendor(args) => rb_core::build::vendor(
            args.path.as_deref(),
            args.no_delete,
            args.manifest_path.as_deref(),
            args.locked || args.frozen,
            args.offline || args.frozen,
            &cfg,
            shell,
        ),
        Cmd::GenerateLockfile(args) => rb_core::build::update(
            &rb_core::build::UpdateOptions {
                specs: Vec::new(),
                precise: None,
                recursive: false,
                dry_run: false,
                manifest_path: args.manifest_path,
                locked: args.locked || args.frozen,
                offline: args.offline || args.frozen,
                ignore_rust_version: false,
            },
            &cfg,
            shell,
        ),
        Cmd::LocateProject(args) => {
            let (package, root) = rb_core::workspace::locate_manifests(&std::env::current_dir()?, args.manifest_path.as_deref())?;
            let path = if args.workspace { root } else { package };
            match args.message_format.as_str() {
                "plain" => println!("{}", path.display()),
                "json" => println!("{}", serde_json::json!({ "root": path })),
                other => anyhow::bail!("unknown --message-format `{other}`"),
            }
            Ok(())
        }
        Cmd::Tree(args) => rb_core::build::tree(
            args.package.as_deref(),
            args.depth,
            &args.edges,
            args.invert.as_deref(),
            &args.prefix,
            &args.format,
            args.charset == "ascii",
            args.manifest_path.as_deref(),
            args.locked || args.frozen,
            args.offline || args.frozen,
            &cfg,
            shell,
        ),
        Cmd::Pkgid(args) => {
            let spec = args.spec.as_deref().or(args.package.as_deref());
            rb_core::build::pkgid(
                spec,
                args.manifest_path.as_deref(),
                args.locked || args.frozen,
                args.offline || args.frozen,
                &cfg,
                shell,
            )
        }
    }
}

fn metadata_options(a: MetadataArgs) -> rb_core::build::MetadataOptions {
    rb_core::build::MetadataOptions {
        format_version: a.format_version,
        no_deps: a.no_deps,
        manifest_path: a.manifest_path,
        locked: a.locked || a.frozen,
        offline: a.offline || a.frozen,
    }
}

fn install_options(a: InstallArgs) -> rb_core::build::InstallOptions {
    rb_core::build::InstallOptions {
        name: a.name,
        version: a.version,
        path: a.path,
        git: a.git,
        branch: a.branch,
        tag: a.tag,
        rev: a.rev,
        bin: a.bin,
        force: a.force,
        root: a.root,
        list: a.list,
        manifest_path: a.manifest_path,
        locked: a.locked || a.frozen,
        offline: a.offline || a.frozen,
        jobs: a.jobs,
    }
}

fn remove_options(a: RemoveArgs) -> rb_core::build::RemoveOptions {
    rb_core::build::RemoveOptions {
        deps: a.deps,
        dev: a.dev,
        build: a.build,
        dry_run: a.dry_run,
        package: a.package,
        manifest_path: a.manifest_path,
        locked: a.locked || a.frozen,
        offline: a.offline || a.frozen,
    }
}

fn add_options(a: AddArgs) -> rb_core::build::AddOptions {
    rb_core::build::AddOptions {
        deps: a.deps,
        path: a.path,
        git: a.git,
        branch: a.branch,
        tag: a.tag,
        rev: a.rev,
        features: a.features,
        no_default_features: a.no_default_features,
        optional: a.optional,
        dev: a.dev,
        build: a.build,
        rename: a.rename,
        dry_run: a.dry_run,
        package: a.package,
        manifest_path: a.manifest_path,
        locked: a.locked || a.frozen,
        offline: a.offline || a.frozen,
        ignore_rust_version: a.ignore_rust_version,
    }
}

fn update_options(a: UpdateArgs) -> Result<rb_core::build::UpdateOptions> {
    let mut specs = a.specs;
    specs.extend(a.package);
    if a.workspace {
        specs.clear();
    }
    Ok(rb_core::build::UpdateOptions {
        specs,
        precise: a.precise,
        recursive: a.recursive,
        dry_run: a.dry_run,
        manifest_path: a.manifest_path,
        locked: a.locked || a.frozen,
        offline: a.offline || a.frozen,
        ignore_rust_version: a.ignore_rust_version,
    })
}

fn store(cmd: StoreCmd, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    let store = rb_store::Store::open(&cfg.store_dir)?;
    match cmd {
        StoreCmd::Path => println!("{}", store.root().display()),
        StoreCmd::Status => {
            let s = rb_store::gc::status(&store)?;
            let projects = std::fs::read_dir(cfg.home.join("projects")).map(|r| r.count()).unwrap_or(0);
            println!("store       {}", store.root().display());
            println!("units       {} ({} variants)", s.units, s.entries);
            println!(
                "blobs       {} totalling {} ({} compressed, unused by any project)",
                s.blobs,
                ui::human_bytes(s.blob_bytes),
                s.compressed_blobs
            );
            println!("  exclusive {} (freed by gc)", ui::human_bytes(s.exclusive_bytes));
            println!("  shared    {} (hardlinked into projects)", ui::human_bytes(s.shared_bytes));
            println!("projects    {projects} registered");
            println!("link mode   {}", cfg.link_mode);
            if let Some(max) = cfg.store_max_size {
                println!("max size    {}", ui::human_bytes(max));
            }
        }
        StoreCmd::Gc {
            max_size,
            max_age_days,
            dry_run,
        } => {
            let _lock = match store.try_gc_lock()? {
                Some(l) => l,
                None => {
                    shell.status("Blocking", "waiting for running builds to release the store");
                    store.gc_lock()?
                }
            };
            let opts = rb_store::gc::GcOptions {
                max_size: match max_size {
                    Some(s) => Some(rb_core::config::parse_size(&s)?),
                    None => cfg.store_max_size,
                },
                max_age_secs: Some(max_age_days.unwrap_or(cfg.gc_max_age_days) * 86400),
                dry_run,
                pinned: rb_core::build::pinned_keys(cfg),
                hot: Some(rb_core::build::project_keys(cfg, None).1),
                ..Default::default()
            };
            let r = rb_store::gc::gc(&store, &opts)?;
            let verb = if dry_run { "Would remove" } else { "Removed" };
            shell.status(
                verb,
                format!(
                    "{} units, {} blobs, {} freed, {} saved by compressing unused blobs ({} units / {} kept)",
                    r.units_removed,
                    r.blobs_removed,
                    ui::human_bytes(r.bytes_freed),
                    ui::human_bytes(r.bytes_compressed),
                    r.units_kept,
                    ui::human_bytes(r.bytes_kept)
                ),
            );
        }
        StoreCmd::Verify { repair } => {
            let r = rb_store::gc::verify(&store, repair)?;
            shell.status("Verified", format!("{} blobs", r.blobs_checked));
            if !r.corrupt_blobs.is_empty() || !r.broken_units.is_empty() {
                let action = if repair { "removed" } else { "found (run with --repair)" };
                shell.warn(format!(
                    "{} corrupt blobs and {} broken units {action}",
                    r.corrupt_blobs.len(),
                    r.broken_units.len()
                ));
                if !repair {
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}
