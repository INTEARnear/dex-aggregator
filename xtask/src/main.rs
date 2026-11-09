use std::{
    env, fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

const HOOKS: [(&str, &str, &str); 2] = [
    ("plach-hook", "libplach.so", "hooks/Plach.so"),
    ("qoq-hook", "libqoq.so", "hooks/Qoq.so"),
];

fn main() -> ExitCode {
    let result = match env::args().skip(1).next().as_deref() {
        Some("hooks") => build_hooks(),
        Some("run") => match build_hooks() {
            Ok(_) => run(),
            Err(err) => Err(err),
        },
        _ => Err("Usage: cargo xtask hooks | cargo xtask run".to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn release_directory() -> PathBuf {
    workspace_root().join("target").join("release")
}

fn build(packages: &[&str]) -> Result<(), String> {
    let cargo = env::var_os("CARGO").ok_or("CARGO isn't set, run this with `cargo xtask`")?;
    for package in packages {
        let status = Command::new(&cargo)
            .current_dir(workspace_root())
            .args(["build", "--release", "--package", package])
            .status()
            .map_err(|err| format!("Can't run cargo: {err}"))?;
        if !status.success() {
            return Err(format!("Building {package} failed"));
        }
    }
    Ok(())
}

fn build_hooks() -> Result<(), String> {
    build(&HOOKS.map(|(package, _, _)| package))?;
    for (_, library, installed) in HOOKS {
        let built = release_directory().join(library);
        let installed = workspace_root().join(installed);
        let copied = installed.with_extension("so.new");
        fs::copy(&built, &copied).map_err(|err| {
            format!(
                "Can't copy {} to {}: {err}",
                built.display(),
                copied.display()
            )
        })?;
        fs::rename(&copied, &installed)
            .map_err(|err| format!("Can't install {}: {err}", installed.display()))?;
        println!("Installed {}", installed.display());
    }
    Ok(())
}

fn run() -> Result<(), String> {
    build(&["swap-router"])?;
    let err = Command::new(release_directory().join("swap-router"))
        .current_dir(workspace_root())
        .exec();
    Err(format!("Can't run swap-router: {err}"))
}
