//! The installation's device id, shared with the plugins:
//! `device.json` beside the store (`{"version":1,"device_id":"<64 hex>"}`,
//! anthropic-auth `device-identity.ts`), read once per process and created
//! when missing, never overwriting one another process wrote first.

use std::io::Write;
use std::path::Path;

use sha2::{Digest, Sha256};

/// The file beside the store.
pub(crate) const DEVICE_FILE: &str = "device.json";
/// The plugins' read limit.
const MAX_BYTES: u64 = 4 * 1024;

/// What `device.json` holds.
enum DeviceFile {
    Missing,
    Unusable,
    Id(String),
}

/// The device id in the directory holding `store_path`, created when
/// missing. `None` when the file is unusable (malformed, a symlink, too
/// large, unwritable): the request then carries no `metadata.user_id`.
pub(crate) fn load_or_create(store_path: &Path) -> Option<String> {
    let directory = store_path.parent()?;
    let path = directory.join(DEVICE_FILE);
    match read(&path) {
        DeviceFile::Id(id) => return Some(id),
        DeviceFile::Unusable => return None,
        DeviceFile::Missing => {}
    }
    std::fs::create_dir_all(directory).ok()?;
    let id = generate();
    let temporary = directory.join(format!(
        "{DEVICE_FILE}.tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        restrict(&temporary);
        file.write_all(format!("{{\"version\":1,\"device_id\":\"{id}\"}}\n").as_bytes())?;
        file.sync_all()?;
        // A link never replaces a file: a peer's id written first wins.
        std::fs::hard_link(&temporary, &path)
    })();
    let _ = std::fs::remove_file(&temporary);
    match (written, read(&path)) {
        (Ok(()), _) => Some(id),
        (Err(_), DeviceFile::Id(id)) => Some(id),
        (Err(_), DeviceFile::Missing | DeviceFile::Unusable) => None,
    }
}

fn read(path: &Path) -> DeviceFile {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return DeviceFile::Missing,
        Err(_) => return DeviceFile::Unusable,
    };
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return DeviceFile::Unusable;
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(|parsed| parsed.get("version").and_then(serde_json::Value::as_u64) == Some(1))
        .and_then(|parsed| {
            ["device_id", "deviceId", "id"].iter().find_map(|key| {
                parsed
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
        })
        .filter(|id| {
            id.len() == 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .map_or(DeviceFile::Unusable, DeviceFile::Id)
}

/// 32 random bytes, lowercase hex.
fn generate() -> String {
    let mut hasher = Sha256::new();
    hasher.update(uuid::Uuid::new_v4().as_bytes());
    hasher.update(uuid::Uuid::new_v4().as_bytes());
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_id_is_created_once_and_read_back() {
        let home = tempfile::tempdir().expect("a temporary home");
        let store = home
            .path()
            .join(".anthropic-accounts")
            .join("accounts.json");

        let created = load_or_create(&store).expect("a device id");
        let text =
            std::fs::read_to_string(home.path().join(".anthropic-accounts").join(DEVICE_FILE))
                .expect("device.json");

        assert_eq!(
            text,
            format!("{{\"version\":1,\"device_id\":\"{created}\"}}\n")
        );
        assert_eq!(load_or_create(&store), Some(created));
    }

    #[test]
    fn a_plugin_written_device_id_is_used_and_a_bad_one_is_not() {
        let home = tempfile::tempdir().expect("a temporary home");
        let store = home.path().join("accounts.json");
        let device = home.path().join(DEVICE_FILE);
        std::fs::write(
            &device,
            format!("{{\"version\":1,\"device_id\":\"{}\"}}\n", "c".repeat(64)),
        )
        .expect("write device.json");
        assert_eq!(load_or_create(&store), Some("c".repeat(64)));

        std::fs::write(&device, "{\"version\":2,\"device_id\":\"x\"}").expect("write device.json");
        assert_eq!(load_or_create(&store), None);
    }
}
