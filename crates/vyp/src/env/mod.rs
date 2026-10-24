//! Creating virtual environments.
//!
//! On POSIX vyp builds the environment itself, the way the stdlib `venv`
//! module does: a `pyvenv.cfg`, interpreter symlinks, the interpreter's own
//! install-scheme directories, and activation scripts. On Windows, where the
//! interpreter launcher must be copied from the base install, it delegates to
//! `python -m venv --without-pip` and then adds vyp's own files.

pub mod provenance;

use std::path::{Path, PathBuf};

use crate::cache::venv::VenvLayout;
use crate::python::Interpreter;

#[derive(Debug, Clone, Default)]
pub struct VenvOptions {
    /// Scripts and activation find the environment relative to themselves,
    /// so the directory can be moved or copied.
    pub relocatable: bool,
    /// Prompt shown by the activation scripts (default: directory name).
    pub prompt: Option<String>,
    /// Replace an existing environment at the path.
    pub clear: bool,
    pub system_site_packages: bool,
}

/// Create a virtual environment at `path` for `interp`.
pub fn create(path: &Path, interp: &Interpreter, opts: &VenvOptions) -> miette::Result<VenvLayout> {
    prepare_target(path, opts.clear)?;
    std::fs::create_dir_all(path)
        .map_err(|e| miette::miette!("Failed to create {}: {}", path.display(), e))?;
    let root = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let prompt = opts.prompt.clone().unwrap_or_else(|| {
        root.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| n != ".venv")
            .or_else(|| {
                root.parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "venv".to_string())
    });

    if cfg!(windows) {
        create_with_stdlib(&root, interp, opts)?;
    } else {
        create_posix(&root, interp, opts, &prompt)?;
    }
    write_pyvenv_cfg(&root, interp, opts, &prompt)?;
    write_file(&root.join(".gitignore"), "# Created by vyp\n*\n")?;
    write_file(
        &root.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n# This directory is a virtual environment created by vyp.\n",
    )?;
    provenance::record_creation(&root, interp, opts)?;

    VenvLayout::from_root(&root)
}

/// Refuse to overwrite anything that isn't a virtual environment.
fn prepare_target(path: &Path, clear: bool) -> miette::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let is_empty = std::fs::read_dir(path).map(|mut d| d.next().is_none()).unwrap_or(false);
    if is_empty {
        return Ok(());
    }
    if !path.join("pyvenv.cfg").exists() {
        return Err(miette::miette!(
            "{} exists and is not a virtual environment; refusing to overwrite it",
            path.display()
        ));
    }
    if !clear {
        return Err(miette::miette!(
            "A virtual environment already exists at {}. Pass --clear to replace it.",
            path.display()
        ));
    }
    std::fs::remove_dir_all(path)
        .map_err(|e| miette::miette!("Failed to remove {}: {}", path.display(), e))
}

fn create_posix(root: &Path, interp: &Interpreter, opts: &VenvOptions, prompt: &str) -> miette::Result<()> {
    let scripts = root.join(&interp.scripts);
    for dir in [&scripts, &root.join(&interp.purelib), &root.join(&interp.include)] {
        std::fs::create_dir_all(dir).map_err(|e| miette::miette!("Failed to create {}: {}", dir.display(), e))?;
    }
    // CPython adds `lib64 -> lib` on 64-bit non-macOS POSIX.
    if cfg!(target_os = "linux") && cfg!(target_pointer_width = "64") && interp.purelib.starts_with("lib/") {
        let lib64 = root.join("lib64");
        if !lib64.exists() {
            symlink(Path::new("lib"), &lib64)?;
        }
    }

    symlink(&interp.base_executable, &scripts.join("python"))?;
    let short = interp.short_version();
    let mut aliases = vec!["python3".to_string(), format!("python{}", short)];
    if interp.free_threaded {
        aliases.push(format!("python{}", short.trim_end_matches('t')));
    }
    for alias in aliases {
        let link = scripts.join(&alias);
        if !link.exists() {
            symlink(Path::new("python"), &link)?;
        }
    }

    write_activation_scripts(root, &scripts, opts.relocatable, prompt)
}

fn create_with_stdlib(root: &Path, interp: &Interpreter, opts: &VenvOptions) -> miette::Result<()> {
    let mut cmd = std::process::Command::new(&interp.base_executable);
    cmd.args(["-m", "venv", "--without-pip"]);
    if opts.system_site_packages {
        cmd.arg("--system-site-packages");
    }
    let status = cmd
        .arg(root)
        .status()
        .map_err(|e| miette::miette!("Failed to run {}: {}", interp.base_executable.display(), e))?;
    if !status.success() {
        return Err(miette::miette!("`python -m venv` failed for {}", root.display()));
    }
    Ok(())
}

fn write_pyvenv_cfg(root: &Path, interp: &Interpreter, opts: &VenvOptions, prompt: &str) -> miette::Result<()> {
    let home = interp
        .base_executable
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut cfg = format!(
        "home = {}\ninclude-system-site-packages = {}\nversion = {}\nexecutable = {}\nimplementation = {}\nvyp = {}\nprompt = {}\n",
        home.display(),
        opts.system_site_packages,
        interp.version,
        interp.base_executable.display(),
        interp.implementation,
        env!("CARGO_PKG_VERSION"),
        prompt,
    );
    if opts.relocatable {
        cfg.push_str("relocatable = true\n");
    }
    write_file(&root.join("pyvenv.cfg"), &cfg)
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn write_activation_scripts(root: &Path, scripts: &Path, relocatable: bool, prompt: &str) -> miette::Result<()> {
    let venv_sh = if relocatable {
        r#"if [ -n "${BASH_SOURCE:-}" ]; then _vyp_src="${BASH_SOURCE[0]}"; elif [ -n "${ZSH_VERSION:-}" ]; then _vyp_src="${(%):-%x}"; else _vyp_src="$0"; fi
VIRTUAL_ENV="$(cd "$(dirname -- "$_vyp_src")/.." && pwd)"
unset _vyp_src"#
            .to_string()
    } else {
        format!("VIRTUAL_ENV={}", sh_quote(&root.display().to_string()))
    };
    let bin = scripts.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "bin".into());
    let activate = format!(
        r#"# This file must be used with "source bin/activate" from bash or zsh.
# Generated by vyp.

deactivate () {{
    if [ -n "${{_OLD_VIRTUAL_PATH:-}}" ] ; then
        PATH="${{_OLD_VIRTUAL_PATH:-}}"
        export PATH
        unset _OLD_VIRTUAL_PATH
    fi
    if [ -n "${{_OLD_VIRTUAL_PYTHONHOME:-}}" ] ; then
        PYTHONHOME="${{_OLD_VIRTUAL_PYTHONHOME:-}}"
        export PYTHONHOME
        unset _OLD_VIRTUAL_PYTHONHOME
    fi
    hash -r 2> /dev/null
    if [ -n "${{_OLD_VIRTUAL_PS1:-}}" ] ; then
        PS1="${{_OLD_VIRTUAL_PS1:-}}"
        export PS1
        unset _OLD_VIRTUAL_PS1
    fi
    unset VIRTUAL_ENV
    unset VIRTUAL_ENV_PROMPT
    if [ ! "${{1:-}}" = "nondestructive" ] ; then
        unset -f deactivate
    fi
}}

deactivate nondestructive

{venv}
export VIRTUAL_ENV

_OLD_VIRTUAL_PATH="$PATH"
PATH="$VIRTUAL_ENV/{bin}:$PATH"
export PATH

VIRTUAL_ENV_PROMPT={prompt}
export VIRTUAL_ENV_PROMPT

if [ -n "${{PYTHONHOME:-}}" ] ; then
    _OLD_VIRTUAL_PYTHONHOME="${{PYTHONHOME:-}}"
    unset PYTHONHOME
fi

if [ -z "${{VIRTUAL_ENV_DISABLE_PROMPT:-}}" ] ; then
    _OLD_VIRTUAL_PS1="${{PS1:-}}"
    PS1="(${{VIRTUAL_ENV_PROMPT}}) ${{PS1:-}}"
    export PS1
fi

hash -r 2> /dev/null
"#,
        venv = venv_sh,
        bin = bin,
        prompt = sh_quote(prompt),
    );
    write_file(&scripts.join("activate"), &activate)?;

    let fish_venv = if relocatable {
        "set -gx VIRTUAL_ENV (builtin realpath (dirname (status -f))/..)".to_string()
    } else {
        format!("set -gx VIRTUAL_ENV {}", sh_quote(&root.display().to_string()))
    };
    let fish = format!(
        r#"# This file must be used with "source bin/activate.fish" from fish.
# Generated by vyp.

function deactivate -d "Exit the virtual environment"
    if test -n "$_OLD_VIRTUAL_PATH"
        set -gx PATH $_OLD_VIRTUAL_PATH
        set -e _OLD_VIRTUAL_PATH
    end
    if test -n "$_OLD_VIRTUAL_PYTHONHOME"
        set -gx PYTHONHOME $_OLD_VIRTUAL_PYTHONHOME
        set -e _OLD_VIRTUAL_PYTHONHOME
    end
    if test -n "$_OLD_FISH_PROMPT_OVERRIDE"
        set -e _OLD_FISH_PROMPT_OVERRIDE
        if functions -q _old_fish_prompt
            functions -e fish_prompt
            functions -c _old_fish_prompt fish_prompt
            functions -e _old_fish_prompt
        end
    end
    set -e VIRTUAL_ENV
    set -e VIRTUAL_ENV_PROMPT
    if test "$argv[1]" != "nondestructive"
        functions -e deactivate
    end
end

deactivate nondestructive

{venv}
set -gx _OLD_VIRTUAL_PATH $PATH
set -gx PATH "$VIRTUAL_ENV/{bin}" $PATH
set -gx VIRTUAL_ENV_PROMPT {prompt}

if set -q PYTHONHOME
    set -gx _OLD_VIRTUAL_PYTHONHOME $PYTHONHOME
    set -e PYTHONHOME
end

if test -z "$VIRTUAL_ENV_DISABLE_PROMPT"
    functions -c fish_prompt _old_fish_prompt
    function fish_prompt
        set -l old_status $status
        printf "(%s) " $VIRTUAL_ENV_PROMPT
        echo "exit $old_status" | .
        _old_fish_prompt
    end
    set -gx _OLD_FISH_PROMPT_OVERRIDE "$VIRTUAL_ENV"
end
"#,
        venv = fish_venv,
        bin = bin,
        prompt = sh_quote(prompt),
    );
    write_file(&scripts.join("activate.fish"), &fish)?;

    // csh can't find the file it is sourced from, so this one always uses
    // the absolute path the environment was created at.
    let csh = format!(
        r#"# This file must be used with "source bin/activate.csh" from csh.
# Generated by vyp.

alias deactivate 'test $?_OLD_VIRTUAL_PATH != 0 && setenv PATH "$_OLD_VIRTUAL_PATH" && unset _OLD_VIRTUAL_PATH; rehash; test $?_OLD_VIRTUAL_PROMPT != 0 && set prompt="$_OLD_VIRTUAL_PROMPT" && unset _OLD_VIRTUAL_PROMPT; unsetenv VIRTUAL_ENV; unsetenv VIRTUAL_ENV_PROMPT; test "\!:*" != "nondestructive" && unalias deactivate'

deactivate nondestructive

setenv VIRTUAL_ENV {venv}
set _OLD_VIRTUAL_PATH="$PATH"
setenv PATH "$VIRTUAL_ENV/{bin}:$PATH"
setenv VIRTUAL_ENV_PROMPT {prompt}

set _OLD_VIRTUAL_PROMPT="$prompt"
if (! "$?VIRTUAL_ENV_DISABLE_PROMPT") then
    set prompt = "($VIRTUAL_ENV_PROMPT) $prompt"
endif

rehash
"#,
        venv = sh_quote(&root.display().to_string()),
        bin = bin,
        prompt = sh_quote(prompt),
    );
    write_file(&scripts.join("activate.csh"), &csh)
}

fn write_file(path: &Path, content: &str) -> miette::Result<()> {
    std::fs::write(path, content).map_err(|e| miette::miette!("Failed to write {}: {}", path.display(), e))
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> miette::Result<()> {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link)
        .map_err(|e| miette::miette!("Failed to link {} -> {}: {}", link.display(), target.display(), e))
}

#[cfg(not(unix))]
fn symlink(target: &Path, link: &Path) -> miette::Result<()> {
    std::fs::copy(target, link)
        .map(|_| ())
        .map_err(|e| miette::miette!("Failed to copy {} -> {}: {}", target.display(), link.display(), e))
}

/// The project's environment: `.venv` next to `pyproject.toml`, unless
/// `VIRTUAL_ENV` points elsewhere or `--venv` was given.
pub fn project_venv_path(explicit: Option<&Path>, project_dir: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(active) = std::env::var("VIRTUAL_ENV") {
        return PathBuf::from(active);
    }
    project_dir.unwrap_or(Path::new(".")).join(".venv")
}

/// The project's environment, created with a suitable interpreter if it
/// doesn't exist yet.
pub fn ensure_project_venv(
    explicit: Option<&Path>,
    project_dir: Option<&Path>,
    python: Option<&str>,
    requires_python: Option<&str>,
) -> miette::Result<VenvLayout> {
    let path = project_venv_path(explicit, project_dir);
    if path.join("pyvenv.cfg").exists() {
        return VenvLayout::discover(Some(&path));
    }
    let request = python.map(crate::python::PythonRequest::parse).unwrap_or(crate::python::PythonRequest::Any);
    let interp = crate::python::find_or_install(&request, requires_python, true)?;
    eprintln!("Creating virtual environment at {} ({})", path.display(), interp.describe());
    create(&path, &interp, &VenvOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vyp-env-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn host_interpreter() -> Option<Interpreter> {
        crate::python::find(&crate::python::PythonRequest::Any, None)
    }

    #[test]
    fn refuses_to_overwrite_non_venv_directories() {
        let dir = scratch("nonvenv");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("important.txt"), "x").unwrap();
        assert!(prepare_target(&dir, true).is_err());
        assert!(dir.join("important.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn creates_a_working_venv() {
        let Some(interp) = host_interpreter() else { return };
        let dir = scratch("create");
        let layout = create(&dir, &interp, &VenvOptions::default()).unwrap();
        assert!(layout.site_packages.is_dir());
        let out = std::process::Command::new(&layout.python_exe)
            .args(["-c", "import sys; print(sys.prefix != sys.base_prefix, sys.prefix)"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.starts_with("True"), "{}", stdout);
        assert!(create(&dir, &interp, &VenvOptions::default()).is_err(), "must not clobber without --clear");
        create(&dir, &interp, &VenvOptions { clear: true, ..Default::default() }).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn relocatable_venv_activates_after_moving() {
        let Some(interp) = host_interpreter() else { return };
        let dir = scratch("reloc");
        create(&dir, &interp, &VenvOptions { relocatable: true, ..Default::default() }).unwrap();
        let moved = scratch("reloc-moved");
        std::fs::rename(&dir, &moved).unwrap();
        let out = std::process::Command::new("bash")
            .args(["-c", &format!(". '{}/bin/activate' && echo \"$VIRTUAL_ENV\" && python -c 'import sys; print(sys.prefix)'", moved.display())])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let real = std::fs::canonicalize(&moved).unwrap();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.first().map(|l| std::fs::canonicalize(l).unwrap()), Some(real.clone()), "{}", stdout);
        assert_eq!(lines.get(1).map(|l| std::fs::canonicalize(l).unwrap()), Some(real), "{}", stdout);
        let _ = std::fs::remove_dir_all(&moved);
    }
}
