//! The reports `spaces attach` and `spaces check` read back from the child
//! that checked a pool (`Report::list_from_json`): any input is read or
//! refused without a panic, and what is read writes back to JSON that reads
//! as the same reports.
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::report::{Report, reports_to_json};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(reports) = Report::list_from_json(text) {
        let again = reports_to_json(&reports);
        let back = Report::list_from_json(&again).expect("written reports read back");
        assert_eq!(reports_to_json(&back), again);
        for r in &reports {
            let _ = r.to_text();
        }
    }
});
