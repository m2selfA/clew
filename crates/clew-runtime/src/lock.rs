use std::{
    fs::{File, OpenOptions, TryLockError},
    io::{Seek, SeekFrom, Write},
};

use clew_core::StateLayout;

pub(crate) enum OwnershipAttempt {
    Acquired(ControllerOwnership),
    Busy,
}

pub(crate) struct ControllerOwnership {
    _file: File,
}

impl ControllerOwnership {
    pub(crate) fn try_acquire(
        layout: &StateLayout,
        instance_id: &str,
    ) -> Result<OwnershipAttempt, std::io::Error> {
        let path = layout.controller_lock_path();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(OwnershipAttempt::Busy),
            Err(TryLockError::Error(error)) => return Err(error),
        }

        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        writeln!(file, "pid={}", std::process::id())?;
        writeln!(file, "instance_id={instance_id}")?;
        file.sync_data()?;

        Ok(OwnershipAttempt::Acquired(Self { _file: file }))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn exclusive_lock_blocks_parallel_owner_and_recovers_from_stale_file() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        std::fs::create_dir_all(layout.version_root()).unwrap();

        let first = match ControllerOwnership::try_acquire(&layout, "first").unwrap() {
            OwnershipAttempt::Acquired(owner) => owner,
            OwnershipAttempt::Busy => panic!("first owner unexpectedly busy"),
        };
        assert!(matches!(
            ControllerOwnership::try_acquire(&layout, "second").unwrap(),
            OwnershipAttempt::Busy
        ));

        drop(first);
        assert!(layout.controller_lock_path().exists());
        assert!(matches!(
            ControllerOwnership::try_acquire(&layout, "third").unwrap(),
            OwnershipAttempt::Acquired(_)
        ));
    }
}
