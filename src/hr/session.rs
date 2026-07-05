use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::util::merged_rustflags;

#[derive(Debug, Clone)]
pub(crate) struct HotSession {
    pub(crate) id: String,
    pub(crate) socket: PathBuf,
}

impl HotSession {
    pub(crate) fn new(workspace_root: &Path) -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!("hr-{}-{nonce}", std::process::id());
        let socket = std::env::temp_dir().join(format!("{id}.sock"));

        if socket.exists() {
            fs::remove_file(&socket)?;
        }

        println!("hr: hot env prepared before Cargo");
        println!("hr: HR_SOCKET={} (reserved)", socket.display());
        println!("hr: HR_WORKSPACE_ROOT={}", workspace_root.display());

        Ok(Self { id, socket })
    }

    pub(crate) fn apply_env(&self, command: &mut Command, workspace_root: &Path) {
        command
            .env("RUSTC_BOOTSTRAP", "1")
            .env("RUSTFLAGS", merged_rustflags())
            .env("HR_SESSION_ID", &self.id)
            .env("HR_SOCKET", &self.socket)
            .env("HR_WORKSPACE_ROOT", workspace_root);
    }
}

pub(crate) fn wait_for_socket(socket: &Path, duration: Duration) -> Result<(), Box<dyn Error>> {
    let deadline = SystemTime::now() + duration;
    loop {
        if socket.exists() {
            return Ok(());
        }
        if SystemTime::now() >= deadline {
            return Err(format!("runtime socket not ready at {}", socket.display()).into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}
