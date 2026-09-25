use std::env;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use wit_component::ComponentEncoder;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    let (pairs, remainder) = arguments.as_chunks::<2>();
    if pairs.is_empty() || !remainder.is_empty() {
        return Err("usage: farever-more-build <core.wasm> <component.wasm> [...]".into());
    }

    for [input, output] in pairs {
        componentize(PathBuf::from(input), PathBuf::from(output))?;
    }
    Ok(())
}

fn componentize(input: PathBuf, output: PathBuf) -> Result<(), Box<dyn Error>> {
    let module = fs::read(&input)?;
    let component = ComponentEncoder::default()
        .module(&module)?
        .validate(true)
        .encode()?;

    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    publish_atomically(&output, &component)?;
    println!("Wasm component: {}", output.display());
    Ok(())
}

fn publish_atomically(output: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = create_temporary_path(parent, output);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, output)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary_path(parent: &Path, output: &Path) -> PathBuf {
    let mut name = output
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("component.wasm"))
        .to_os_string();
    name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    parent.join(name)
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, output: &Path) -> io::Result<()> {
    fs::rename(temporary, output)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, output: &Path) -> io::Result<()> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let output = output
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            temporary.as_ptr(),
            output.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_publication_replaces_the_destination_and_removes_staging_file() {
        let root = std::env::temp_dir().join(format!(
            "farever-more-build-test-{}-{}",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create test directory");
        let output = root.join("addon.wasm");
        fs::write(&output, b"old").expect("write existing component");

        publish_atomically(&output, b"new").expect("replace component atomically");

        assert_eq!(fs::read(&output).expect("read replacement"), b"new");
        assert_eq!(fs::read_dir(&root).expect("read test directory").count(), 1);
        fs::remove_dir_all(root).expect("remove test directory");
    }
}
