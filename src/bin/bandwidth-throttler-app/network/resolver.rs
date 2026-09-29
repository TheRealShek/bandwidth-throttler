//! Owns the resolver file that `ip netns exec` binds into the app namespace.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use crate::Result;

pub(super) const RESOLVER_PARENT: &str = "/etc/netns";
pub(super) const RESOLVER_DIR: &str = "/etc/netns/bt-app-v1";
const RESOLVER_FILE: &str = "/etc/netns/bt-app-v1/resolv.conf";
const OWNER_FILE: &str = "/etc/netns/.bt-app-v1-owner";
const OWNER_TEMP: &str = "/etc/netns/.bt-app-v1-owner.tmp";
const OWNER_MAGIC: &str = "bandwidth-throttler-app resolver v2";
const BOOT_ID_FILE: &str = "/proc/sys/kernel/random/boot_id";
const RANDOM_ID_FILE: &str = "/proc/sys/kernel/random/uuid";

struct Owner {
    boot_id: String,
    parent_created: bool,
    token: String,
}

/// Writes persistent ownership before creating the resolver directory.
pub(super) fn create(dns: Ipv4Addr, parent_created: bool) -> Result<()> {
    fs::create_dir_all(RESOLVER_PARENT)?;
    let token = read_id(RANDOM_ID_FILE)?;
    if !valid_token(&token) {
        return Err("Cannot read a valid resolver ownership token.".into());
    }
    let owner = Owner {
        boot_id: current_boot_id()?,
        parent_created,
        token,
    };
    owner.save()?;
    let staging_dir = staging_dir(&owner.token);
    fs::create_dir(&staging_dir)?;
    let staging_file = format!("{staging_dir}/resolv.conf");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o644)
        .open(&staging_file)?;
    writeln!(file, "# bandwidth-throttler-app {}", owner.token)?;
    writeln!(file, "nameserver {dns}")?;
    // sudo may inherit a restrictive umask, but the selected app reads this file as a user.
    fs::set_permissions(&staging_file, fs::Permissions::from_mode(0o644))?;
    file.sync_all()?;
    File::open(&staging_dir)?.sync_all()?;
    // Publish the directory only after its resolver file is complete.
    fs::rename(&staging_dir, RESOLVER_DIR)?;
    File::open(RESOLVER_PARENT)?.sync_all()?;
    Ok(())
}

/// Removes only the resolver paths created by this launcher.
pub(super) fn cleanup(parent_created: bool) -> Result<()> {
    let mut errors = Vec::new();
    let owner = if exists(OWNER_FILE)? {
        Some(Owner::load()?)
    } else {
        None
    };
    if exists(RESOLVER_DIR)? {
        let owner = owner
            .as_ref()
            .ok_or("Resolver directory exists without an owner record; leaving it untouched.")?;
        validate_published(owner)?;
        let staging_dir = staging_dir(&owner.token);
        if exists(&staging_dir)? {
            return Err(
                "Both live and staged resolver directories exist; leaving them untouched.".into(),
            );
        }
        // Keep an incomplete cleanup under the tokened path, where reboot recovery can find it.
        fs::rename(RESOLVER_DIR, staging_dir)?;
        File::open(RESOLVER_PARENT)?.sync_all()?;
    }
    if let Some(owner) = &owner {
        let staging_dir = staging_dir(&owner.token);
        validate_staged(owner)?;
        if let Err(error) = remove_file_if_present(&format!("{staging_dir}/resolv.conf")) {
            errors.push(error.to_string());
        }
        if let Err(error) = remove_dir_if_present(&staging_dir) {
            errors.push(error.to_string());
        }
    }
    // Keep the owner record if the resolver directory still needs recovery.
    if errors.is_empty() {
        for path in [OWNER_FILE, OWNER_TEMP] {
            if let Err(error) = remove_file_if_present(path) {
                errors.push(error.to_string());
            }
        }
    }
    if parent_created && errors.is_empty() {
        match fs::remove_dir(RESOLVER_PARENT) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}

/// Removes persistent resolver files only after their owning boot has ended.
pub(super) fn recover_after_reboot() -> Result<()> {
    if !exists(OWNER_FILE)? {
        if exists(OWNER_TEMP)? && !exists(RESOLVER_DIR)? {
            remove_file_if_present(OWNER_TEMP)?;
        }
        return Ok(());
    }
    let owner = Owner::load()?;
    if owner.boot_id == current_boot_id()? {
        return Err("Resolver owner exists in this boot without recovery state; leaving network resources untouched.".into());
    }
    if exists(RESOLVER_DIR)? {
        validate_published(&owner)?;
    }
    let staging_dir = staging_dir(&owner.token);
    if exists(&staging_dir)? {
        validate_staged(&owner)?;
    }
    cleanup(owner.parent_created)
}

fn validate_staged(owner: &Owner) -> Result<()> {
    let staging_dir = staging_dir(&owner.token);
    if !exists(&staging_dir)? {
        return Ok(());
    }
    let directory = fs::symlink_metadata(&staging_dir)?;
    if !directory.file_type().is_dir() || directory.uid() != 0 {
        return Err("Staged resolver directory ownership changed; leaving it untouched.".into());
    }
    let staging_file = format!("{staging_dir}/resolv.conf");
    if exists(&staging_file)? {
        let file = fs::symlink_metadata(&staging_file)?;
        if !file.file_type().is_file() || file.uid() != 0 {
            return Err("Staged resolver file ownership changed; leaving it untouched.".into());
        }
        let contents = fs::read(&staging_file)?;
        let header = format!("# bandwidth-throttler-app {}", owner.token);
        let complete_header = format!("{header}\n");
        if !header.as_bytes().starts_with(&contents)
            && !contents.starts_with(complete_header.as_bytes())
        {
            return Err("Staged resolver ownership token changed; leaving it untouched.".into());
        }
    }
    Ok(())
}

fn validate_published(owner: &Owner) -> Result<()> {
    let directory = fs::symlink_metadata(RESOLVER_DIR)?;
    if !directory.file_type().is_dir() || directory.uid() != 0 {
        return Err("Resolver directory ownership changed; leaving it untouched.".into());
    }
    let file = fs::symlink_metadata(RESOLVER_FILE)?;
    if !file.file_type().is_file() || file.uid() != 0 {
        return Err("Resolver file ownership changed; leaving it untouched.".into());
    }
    let contents = fs::read_to_string(RESOLVER_FILE)?;
    let expected = format!("# bandwidth-throttler-app {}", owner.token);
    if contents.lines().next() != Some(expected.as_str()) {
        return Err("Resolver ownership token changed; leaving it untouched.".into());
    }
    Ok(())
}

impl Owner {
    fn save(&self) -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(OWNER_TEMP)?;
        writeln!(file, "{OWNER_MAGIC}")?;
        writeln!(file, "{}", self.boot_id)?;
        writeln!(file, "{}", u8::from(self.parent_created))?;
        writeln!(file, "{}", self.token)?;
        file.sync_all()?;
        fs::rename(OWNER_TEMP, OWNER_FILE)?;
        File::open(RESOLVER_PARENT)?.sync_all()?;
        Ok(())
    }

    fn load() -> Result<Self> {
        let metadata = fs::symlink_metadata(OWNER_FILE)?;
        if !metadata.file_type().is_file() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
            return Err("Unsafe resolver owner file; leaving it untouched.".into());
        }
        let contents = fs::read_to_string(OWNER_FILE)?;
        let mut lines = contents.lines();
        if lines.next() != Some(OWNER_MAGIC) {
            return Err("Invalid resolver owner file; leaving it untouched.".into());
        }
        let boot_id = lines.next().ok_or("Incomplete resolver owner file")?;
        let parent_created = lines.next().ok_or("Incomplete resolver owner file")?;
        let token = lines.next().ok_or("Incomplete resolver owner file")?;
        if boot_id.is_empty()
            || !matches!(parent_created, "0" | "1")
            || !valid_token(token)
            || lines.next().is_some()
        {
            return Err("Invalid resolver owner file; leaving it untouched.".into());
        }
        Ok(Self {
            boot_id: boot_id.to_owned(),
            parent_created: parent_created == "1",
            token: token.to_owned(),
        })
    }
}

fn staging_dir(token: &str) -> String {
    format!("{RESOLVER_PARENT}/.bt-app-v1-stage-{token}")
}

fn valid_token(token: &str) -> bool {
    token.len() == 36
        && token.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn current_boot_id() -> Result<String> {
    read_id(BOOT_ID_FILE)
}

fn read_id(path: &str) -> Result<String> {
    let id = fs::read_to_string(path)?;
    let id = id.trim();
    if id.is_empty() {
        return Err(format!("Cannot read an ID from {path}.").into());
    }
    Ok(id.to_owned())
}

fn exists(path: &str) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn remove_file_if_present(path: &str) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_dir_if_present(path: &str) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
