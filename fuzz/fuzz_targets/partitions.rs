//! The partition tables of a device or space as the guard reads them (GPT,
//! its backup, MBR), and the search for the Storage Spaces partition on an
//! arbitrary device.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::gpt::{check_partitions, find_spaces_partition};
use storage_spaces::io::MemDevice;

fuzz_target!(|data: &[u8]| {
    let dev = MemDevice(data.to_vec());
    for sector in [512, 4096] {
        let parts = storage_spaces::guard::partitions(&dev, data.len() as u64, sector);
        let _ = check_partitions(&parts, data.len() as u64);
    }
    let _ = find_spaces_partition(&dev);
});
