//! M2's gate (§9): the fakes pass every conformance suite. Each suite is
//! the contract's numbered promises as executable law; the local
//! implementations (cascade-blockdev M3, cascade-logstore M4) run these
//! same functions against their own harnesses.

use cascade_storage_api::conformance::{
    block_device_suite, key_provider_suite, log_sink_suite, log_source_suite,
    page_source_suite, MemBlockDeviceHarness, MemKeyProviderHarness, MemLogStoreHarness,
    MemPageStoreHarness,
};

#[test]
fn the_fakes_pass_the_log_store_suites() {
    let mut h = MemLogStoreHarness;
    log_sink_suite(&mut h);
    log_source_suite(&mut h);
}

#[test]
fn the_fakes_pass_the_block_device_suite() {
    let mut h = MemBlockDeviceHarness;
    block_device_suite(&mut h);
}

#[test]
fn the_fakes_pass_the_key_provider_suite() {
    let mut h = MemKeyProviderHarness;
    key_provider_suite(&mut h);
}

#[test]
fn the_fakes_pass_the_page_source_suite() {
    let mut h = MemPageStoreHarness;
    page_source_suite(&mut h);
}
