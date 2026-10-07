//! Ordinary ambient helpers: independent of confined private worker dispatch.
mod which;
use std::ffi::OsStr;
use std::path::Path;
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let name = Path::new(&args[0])
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let code = match name.as_str() {
        "cat" => uu_cat::uumain(std::env::args_os()),
        "head" => uu_head::uumain(std::env::args_os()),
        "tail" => uu_tail::uumain(std::env::args_os()),
        "wc" => uu_wc::uumain(std::env::args_os()),
        "ls" => uu_ls::uumain(std::env::args_os()),
        "mkdir" => uu_mkdir::uumain(std::env::args_os()),
        "rm" => uu_rm::uumain(std::env::args_os()),
        "cp" => uu_cp::uumain(std::env::args_os()),
        "mv" => uu_mv::uumain(std::env::args_os()),
        "sort" => uu_sort::uumain(std::env::args_os()),
        "uniq" => uu_uniq::uumain(std::env::args_os()),
        "tr" => uu_tr::uumain(std::env::args_os()),
        "cut" => uu_cut::uumain(std::env::args_os()),
        "echo" => uu_echo::uumain(std::env::args_os()),
        "grep" => grep(&args[1..]),
        "which" => which::run(&args[1..]),
        _ => {
            eprintln!(
                "Invoke an installed tool name (cat, head, grep, ...), not newt-carried-tools."
            );
            2
        }
    };
    std::process::exit(code);
}
fn grep(args: &[std::ffi::OsString]) -> i32 {
    let translated = match newt_carried_tools::translate(args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("grep: {error}");
            return 2;
        }
    };
    let result = std::env::current_exe().and_then(|exe| {
        let rg = exe
            .parent()
            .expect("executable has parent")
            .join(if cfg!(windows) { "rg.exe" } else { "rg" });
        let mut command = std::process::Command::new(rg);
        command.args(translated.args);
        if translated.suppress_stdout {
            command.stdout(std::process::Stdio::null());
        }
        command.status()
    });
    match result {
        Ok(status) => status.code().unwrap_or(2),
        Err(error) => {
            eprintln!(
                "grep: adjacent ripgrep could not run: {error}; reinstall the carried tools pack"
            );
            2
        }
    }
}
