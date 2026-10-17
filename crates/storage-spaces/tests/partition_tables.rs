//! A space whose partition table lost its start to a block of another
//! disk, as an oracle mutant of the 1.2.2 campaign did to ntfsparity (user
//! mode #92: the 4 KiB at 0x61000000 of its third disk, the space's MBR and
//! primary GPT header, overwritten by those of the second;
//! testdata/pools, skipped when missing). The guard reads the backup GPT,
//! says suspect, and still checks the NTFS volume of the partition it
//! names; 1.2.2 said healthy, "no partition table".

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use storage_spaces::Pool;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{Overlay, ReadAt, WriteAt};
use storage_spaces::report::{Report, Status, Verdict};

fn report(pool: &Pool<Overlay<Arc<File>>>) -> Report {
    let space = pool.user_spaces().next().unwrap();
    guard::space_report(pool, space, &[], &Options::default(), &mut guard::ntfs_hook)
}

#[test]
fn a_space_whose_partition_table_lost_its_start_is_suspect() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pools/ntfsparity");
    let files: Vec<Arc<File>> = (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.img"))).ok())
        .map(Arc::new)
        .collect();
    if files.is_empty() {
        eprintln!("{} missing, skipped", dir.display());
        return;
    }
    let devices = || -> Vec<Overlay<Arc<File>>> { files.iter().map(|f| Overlay::new(Arc::clone(f))).collect() };
    let before = report(&Pool::open(devices()).unwrap());
    assert_eq!(before.verdict(), Verdict::Healthy, "{}", before.to_text());
    let ntfs = |r: &Report| r.checks.iter().filter(|c| c.id == "fs.ntfs").count();
    assert!(ntfs(&before) > 0, "{}", before.to_text());

    let devs = devices();
    let mut page = vec![0u8; 4096];
    devs[1].read_exact_at(&mut page, 0x6100_0000).unwrap();
    devs[2].write_all_at(&page, 0x6100_0000).unwrap();
    let after = report(&Pool::open(devs).unwrap());
    let text = after.to_text();
    let table = after.checks.iter().find(|c| c.id == "space.partitions").unwrap();
    assert_eq!(table.status, Status::Suspect, "{text}");
    assert!(table.summary.contains("primary GPT header is lost"), "{text}");
    assert_eq!(after.verdict(), Verdict::Suspect, "{text}");
    // The partitions come from the backup: their file systems are checked.
    assert_eq!(ntfs(&after), ntfs(&before), "{text}");
}
