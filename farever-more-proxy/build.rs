fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("manifest directory");
    let output = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("output directory"))
        .join("bootstrap.obj");
    let compiler = find_msvc_compiler().unwrap_or_else(|| std::path::PathBuf::from("cl.exe"));
    let source = std::path::Path::new(&manifest)
        .join("src")
        .join("bootstrap.c");
    let status = std::process::Command::new(compiler)
        .arg("/nologo")
        .arg("/c")
        .arg("/O2")
        .arg("/GS-")
        .arg(format!("/Fo{}", output.display()))
        .arg(&source)
        .status()
        .expect("run the MSVC C compiler");
    assert!(status.success(), "compile bootstrap.c");
    println!("cargo:rustc-link-arg={}", output.display());

    let definition = std::path::Path::new(&manifest)
        .join("src")
        .join("dinput8.def");
    println!("cargo:rustc-link-arg=/DEF:{}", definition.display());
    println!("cargo:rerun-if-changed=src/dinput8.def");
    println!("cargo:rerun-if-changed=src/bootstrap.c");
}

fn find_msvc_compiler() -> Option<std::path::PathBuf> {
    if let Some(root) = std::env::var_os("VCToolsInstallDir") {
        let path = std::path::PathBuf::from(root)
            .join("bin")
            .join("Hostx64")
            .join("x64")
            .join("cl.exe");
        if path.is_file() {
            return Some(path);
        }
    }
    let program_files = std::env::var_os("ProgramFiles(x86)")?;
    let vswhere = std::path::PathBuf::from(program_files)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    let result = std::process::Command::new(vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()
        .ok()?;
    let installation = String::from_utf8(result.stdout).ok()?.trim().to_owned();
    let tools = std::path::Path::new(&installation)
        .join("VC")
        .join("Tools")
        .join("MSVC");
    let mut versions = std::fs::read_dir(tools)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    versions.sort();
    versions.into_iter().rev().find_map(|version| {
        let compiler = version
            .join("bin")
            .join("Hostx64")
            .join("x64")
            .join("cl.exe");
        compiler.is_file().then_some(compiler)
    })
}
