use anyhow::{Context, Result};
use rga::adapters::custom::map_exe_error;
use rga::adapters::*;
use rga::config::{RgaConfig, split_args};
use rga::matching::*;
use rga::preproc::*;
use rga::print_dur;
use ripgrep_all as rga;
use structopt::StructOpt;

use log::debug;
use schemars::schema_for;
use std::process::{Command, Stdio};
use std::time::Instant;
use tokio::fs::File;

fn list_adapters(args: RgaConfig) -> Result<()> {
    let (enabled_adapters, disabled_adapters) = get_all_adapters(args.custom_adapters);

    println!("Adapters:\n");
    let print = |adapter: std::sync::Arc<dyn FileAdapter>| {
        let meta = adapter.metadata();
        let matchers = meta
            .fast_matchers
            .iter()
            .map(|m| match m {
                FastFileMatcher::FileExtension(ext) => format!(".{ext}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let slow_matchers = meta
            .slow_matchers
            .as_ref()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|m| match m {
                FileMatcher::MimeType(x) => Some(x.to_string()),
                FileMatcher::Fast(_) => None,
            })
            .collect::<Vec<_>>()
            .join(", ");
        print!(
            " - **{name}**\n     {desc}  \n     Extensions: {matchers}  \n     Mime Types: {mime}  \n",
            name = meta.name,
            desc = meta.description.replace('\n', "\n     "),
            matchers = matchers,
            mime = slow_matchers,
        );
        println!();
    };
    for adapter in enabled_adapters {
        print(adapter)
    }
    println!(
        "The following adapters are disabled by default, and can be enabled using '--rga-adapters=+foo,bar':\n"
    );
    for adapter in disabled_adapters {
        print(adapter)
    }
    Ok(())
}
/// Determine the mode based on how the binary was invoked
fn get_invocation_mode() -> &'static str {
    // First check argv[0] (for symlink support - this is how busybox works)
    let args: Vec<String> = std::env::args().collect();
    if !args.is_empty() {
        let argv0 = &args[0];
        if argv0.contains("rga-preproc") {
            return "preproc";
        } else if argv0.contains("rga-fzf-open") {
            return "fzf-open";
        } else if argv0.contains("rga-fzf") {
            return "fzf";
        }
    }

    // Check for subcommands
    if args.len() > 1 {
        match args[1].as_str() {
            "preproc" => return "preproc",
            "fzf" => return "fzf",
            "fzf-open" => return "fzf-open",
            _ => {}
        }
    }

    // Check if being called by ripgrep as a preprocessor via environment variable
    if std::env::var("RGA_PREPROC_MODE").is_ok() {
        return "preproc";
    }

    // Check if being called by ripgrep as a preprocessor
    // When ripgrep calls the --pre command, it passes only the filename
    // If we have exactly 1 argument (after program name) and it's an existing file path, assume preproc mode
    if args.len() == 2 {
        let potential_file = &args[1];
        // Only treat as preproc if it's not a flag and the file exists
        if !potential_file.starts_with('-') && std::path::Path::new(potential_file).exists() {
            return "preproc";
        }
    }

    "main"
}

fn main() -> anyhow::Result<()> {
    // set debugging as early as possible
    if std::env::args().any(|e| e == "--debug") {
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("RUST_LOG", "debug") };
    }

    env_logger::init();

    // Determine which mode to run in
    let mode = get_invocation_mode();
    
    match mode {
        "preproc" => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(run_preproc())
        }
        "fzf" => run_fzf(),
        "fzf-open" => run_fzf_open(),
        _ => run_main(),
    }
}

/// Run the main rga search functionality
fn run_main() -> anyhow::Result<()> {
    let (config, mut passthrough_args) = split_args(false)?;

    if config.print_config_schema {
        println!("{}", serde_json::to_string_pretty(&schema_for!(RgaConfig))?);
        return Ok(());
    }
    if config.list_adapters {
        return list_adapters(config);
    }
    if let Some(path) = config.fzf_path {
        if path == "_" {
            // fzf found no result, ignore everything and return
            println!("[no file found]");
            return Ok(());
        }
        passthrough_args.push(std::ffi::OsString::from(&path[1..]));
    }

    if passthrough_args.is_empty() {
        // rg would show help. Show own help instead.
        RgaConfig::clap().print_help()?;
        println!();
        return Ok(());
    }

    let adapters = get_adapters_filtered(config.custom_adapters.clone(), &config.adapters)?;

    let pre_glob = if !config.accurate {
        let extensions = adapters
            .iter()
            .flat_map(|a| &a.metadata().fast_matchers)
            .flat_map(|m| match m {
                FastFileMatcher::FileExtension(ext) => vec![ext.clone(), ext.to_ascii_uppercase()],
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("*.{{{extensions}}}")
    } else {
        "*".to_owned()
    };

    add_exe_to_path()?;

    let rg_args = vec![
        "--no-line-number",
        // smart case by default because within weird files
        // we probably can't really trust casing anyways
        "--smart-case",
    ];

    let exe = std::env::current_exe().expect("Could not get executable location");
    // Use the same executable for preprocessing with "preproc" subcommand
    let preproc_exe = &exe;

    let before = Instant::now();
    let mut cmd = Command::new("rg");
    cmd.args(rg_args)
        .arg("--pre")
        .arg(preproc_exe)
        .arg("--pre-glob")
        .arg(pre_glob)
        .args(passthrough_args)
        .env("RGA_PREPROC_MODE", "1"); // Signal to child processes that they should run in preproc mode
    log::debug!("rg command to run: {:?}", cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| map_exe_error(e, "rg", "Please make sure you have ripgrep installed."))?;

    let result = child.wait()?;

    log::debug!("running rg took {}", print_dur(before));
    if !result.success() {
        std::process::exit(result.code().unwrap_or(1));
    }
    Ok(())
}

/// add the directory that contains `rga` to PATH, so rga-preproc can find pandoc etc (if we are on Windows where we include dependent binaries)
fn add_exe_to_path() -> Result<()> {
    use std::env;
    let mut exe = env::current_exe().expect("Could not get executable location");
    // let preproc_exe = exe.with_file_name("rga-preproc");
    exe.pop(); // dirname

    let path = env::var_os("PATH").unwrap_or_default();
    let paths = env::split_paths(&path).collect::<Vec<_>>();
    // prepend: prefer bundled versions to system-installed versions of binaries
    // solves https://github.com/phiresky/ripgrep-all/issues/32
    // may be somewhat of a security issue if rga binary is in installed in unprivileged locations
    let paths = [&[exe.to_owned(), exe.join("lib")], &paths[..]].concat();
    let new_path = env::join_paths(paths)?;
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { env::set_var("PATH", new_path) };
    Ok(())
}

/// Run the preprocessor functionality (rga-preproc)
async fn run_preproc() -> anyhow::Result<()> {
    let mut arg_arr: Vec<std::ffi::OsString> = std::env::args_os().collect();
    
    // Remove "preproc" subcommand if present
    if arg_arr.len() > 1 {
        let second_arg = arg_arr[1].to_string_lossy();
        if second_arg == "preproc" {
            arg_arr.remove(1);
        }
    }
    
    let last = arg_arr.pop().expect("No filename specified");
    let config = rga::config::parse_args(arg_arr, true)?;
    //clap::App::new("rga-preproc").arg(Arg::from_usage())
    let path = {
        let filepath = last;
        std::env::current_dir()?.join(filepath)
    };

    let i = File::open(&path)
        .await
        .context("Specified input file not found")?;
    let mut o = tokio::io::stdout();
    let ai = AdaptInfo {
        inp: Box::pin(i),
        filepath_hint: path,
        is_real_file: true,
        line_prefix: "".to_string(),
        archive_recursion_depth: 0,
        postprocess: !config.no_prefix_filenames,
        config,
    };

    let start = Instant::now();
    let mut oup = rga_preproc(ai).await.context("during preprocessing")?;
    debug!("finding and starting adapter took {}", print_dur(start));
    let res = tokio::io::copy(&mut oup, &mut o).await;
    if let Err(e) = res {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            // happens if e.g. ripgrep detects binary data in the pipe so it cancels reading
            debug!("output cancelled (broken pipe)");
        } else {
            Err(e).context("copying adapter output to stdout")?;
        }
    }
    debug!("running adapter took {} total", print_dur(start));
    Ok(())
}

/// Run the fzf integration functionality (rga-fzf)
fn run_fzf() -> anyhow::Result<()> {
    let mut passthrough_args: Vec<String> = std::env::args().skip(1).collect();
    
    // Remove "fzf" subcommand if present
    if !passthrough_args.is_empty() && passthrough_args[0] == "fzf" {
        passthrough_args.remove(0);
    }
    
    let inx = passthrough_args.iter().position(|e| !e.starts_with('-'));
    let initial_query = if let Some(inx) = inx {
        passthrough_args.remove(inx)
    } else {
        "".to_string()
    };

    let exe = std::env::current_exe().context("Could not get executable location")?;
    let preproc_exe = exe
        .to_str()
        .context("rga executable is in non-unicode path")?;
    let open_exe = preproc_exe; // Use the same binary

    let rg_prefix = format!("{preproc_exe} --files-with-matches --rga-cache-max-blob-len=10M");

    let child = Command::new("fzf")
        .arg(format!(
            "--preview={preproc_exe} --pretty --context 5 {{q}} --rga-fzf-path=_{{}}"
        ))
        .arg("--preview-window=70%:wrap")
        .arg("--phony")
        .arg("--query")
        .arg(&initial_query)
        .arg("--print-query")
        .arg(format!("--bind=change:reload: {rg_prefix} {{q}}"))
        .arg(format!("--bind=ctrl-m:execute:{open_exe} fzf-open {{q}} {{}}"))
        .env(
            "FZF_DEFAULT_COMMAND",
            format!("{} '{}'", rg_prefix, &initial_query),
        )
        .env("RGA_FZF_INSTANCE", format!("{}", std::process::id())) // may be useful to open stuff in the same tab
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| map_exe_error(e, "fzf", "Please make sure you have fzf installed."))?;

    let output = child.wait_with_output()?;
    let mut x = output.stdout.split(|e| e == &b'\n');
    let final_query =
        std::str::from_utf8(x.next().context("fzf output empty")?).context("fzf query not utf8")?;
    let selected_file = std::str::from_utf8(x.next().context("fzf output not two line")?)
        .context("fzf ofilename not utf8")?;
    println!("query='{final_query}', file='{selected_file}'");

    Ok(())
}

/// Run the fzf file opener functionality (rga-fzf-open)
fn run_fzf_open() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    
    // Remove "fzf-open" subcommand if present
    let first_arg = args.next().context("no first argument")?;
    let (query, fname) = if first_arg == "fzf-open" {
        (args.next().context("no query")?, args.next().context("no filename")?)
    } else {
        (first_arg, args.next().context("no filename")?)
    };
    
    // let instance_id = std::env::var("RGA_FZF_INSTANCE").unwrap_or("unk".to_string());

    if fname.ends_with(".pdf") {
        use std::io::ErrorKind::*;

        let worked = Command::new("evince")
            .arg("--find")
            .arg(&query)
            .arg(&fname)
            .spawn()
            .map_or_else(
                |err| match err.kind() {
                    NotFound => Ok(false),
                    _ => Err(err),
                },
                |_| Ok(true),
            )?;
        if worked {
            return Ok(());
        }
    }
    Ok(open::that_detached(&fname)?)
}
