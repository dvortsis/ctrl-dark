use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icons/CtrlDark.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = match env::var_os("CARGO_MANIFEST_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => return,
    };
    let ico_path = manifest_dir
        .join("assets")
        .join("icons")
        .join("CtrlDark.ico");
    if !ico_path.exists() {
        println!(
            "cargo:warning=Icon file not found at {}; skipping Windows icon resource.",
            ico_path.display()
        );
        return;
    }

    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=Windows SDK rc.exe not found; skipping native .exe icon resource.");
        return;
    };

    let out_dir = match env::var_os("OUT_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => return,
    };
    let rc_file = out_dir.join("ctrldark.rc");
    let res_file = out_dir.join("ctrldark.res");

    let ico_escaped = ico_path.to_string_lossy().replace('\\', "/");
    let rc_body = format!("1 ICON \"{ico_escaped}\"\n");
    if let Err(err) = fs::write(&rc_file, rc_body) {
        println!("cargo:warning=Failed writing {}: {err}", rc_file.display());
        return;
    }

    match Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .output()
    {
        Ok(out) if out.status.success() && res_file.exists() => {
            println!("cargo:rustc-link-arg-bins={}", res_file.display());
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            println!(
                "cargo:warning=rc.exe failed ({:?}): {} {}",
                out.status.code(),
                stdout.trim(),
                stderr.trim()
            );
        }
        Err(err) => {
            println!("cargo:warning=Failed running {}: {err}", rc_exe.display());
        }
    }
}

fn find_rc_exe() -> Option<PathBuf> {
    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let kits_root = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let entries = fs::read_dir(kits_root).ok()?;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path().join("x64").join("rc.exe");
        if p.is_file() {
            candidates.push(p);
        }
    }
    candidates.sort();
    candidates.pop()
}
