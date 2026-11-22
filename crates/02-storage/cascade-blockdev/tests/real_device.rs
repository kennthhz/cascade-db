//! M3's gate (§9): the real glommio device passes the same block-device
//! conformance suite the fake passes — under a real executor, on real
//! O_DIRECT files. Crash cut points and sync-failure injection cannot be
//! forced onto real hardware, so the harness opts out honestly (§4.10:
//! the fake owns those sections); what the suite still proves here is
//! alignment, read-back, synced-write survival, rename/remove/list
//! atomicity — every promise a real device can answer.

use std::path::PathBuf;
use std::sync::Arc;

use cascade_blockdev::GlommioBlockDevice;
use cascade_storage_api::conformance::{
    block_device_suite, BlockDeviceHarness,
};

struct RealHarness {
    dir: PathBuf,
}

impl BlockDeviceHarness for RealHarness {
    type Dev = GlommioBlockDevice;

    fn create(&mut self) -> Arc<Self::Dev> {
        Arc::new(GlommioBlockDevice::new(self.dir.clone()))
    }

    /// A real crash cannot be staged from inside the process. The harness
    /// records the intent: the suite's synced-survival assertions still
    /// run (and hold trivially), while the cut-point and S19 sections
    /// belong to the fake. Crash consistency of the real log store is
    /// M4's gate, on MemBlockDevice.
    fn crash(&mut self, _dev: &Arc<Self::Dev>, _seed: u64) {}

    fn inject_sync_failure(&mut self, _dev: &Arc<Self::Dev>) {}

    fn supports_sync_injection(&self) -> bool {
        false
    }
}

#[test]
fn the_real_device_passes_the_block_device_suite() {
    // A fresh directory per run — O_DIRECT on the workspace filesystem
    // (WSL2 ext4), not on tmpfs, which refuses it.
    let dir = std::env::temp_dir().join(format!(
        "cascade-blockdev-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("test dir");
    let mut h = RealHarness { dir: dir.clone() };
    glommio::LocalExecutor::default().run(async {
        block_device_suite(&mut h).await;
    });
    std::fs::remove_dir_all(&dir).expect("cleanup");
}
