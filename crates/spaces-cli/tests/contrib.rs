//! The files in contrib/ that the package and contrib/install.sh install.

use std::path::Path;

/// The attach unit points to this project's repository (1.1.0 still had
/// its old address) and to the man page.
#[test]
fn the_attach_unit_documents_this_project() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/systemd/storage-spaces-attach.service");
    let unit = std::fs::read_to_string(path).unwrap();
    let docs: Vec<&str> = unit
        .lines()
        .filter_map(|l| l.strip_prefix("Documentation="))
        .flat_map(|l| l.split(' '))
        .collect();
    assert!(docs.contains(&env!("CARGO_PKG_REPOSITORY")), "{docs:?}");
    assert!(docs.contains(&"man:spaces(8)"), "{docs:?}");
}

/// The attach unit runs before local-fs-pre.target (so that fstab can mount
/// volumes of spaces) and must not order itself after anything that comes
/// after it: PrivateTmp= orders it after systemd-tmpfiles-setup.service,
/// which comes after local-fs.target. 1.1.0 and 1.2.0 had that cycle, and
/// systemd broke it at every boot by dropping jobs, other units' mounts
/// among them.
#[test]
fn the_attach_unit_has_no_ordering_cycle_at_boot() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/systemd/storage-spaces-attach.service");
    let unit = std::fs::read_to_string(path).unwrap();
    let settings: Vec<&str> = unit
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .collect();
    assert!(settings.contains(&"Before=local-fs-pre.target"), "{unit}");
    assert!(settings.contains(&"DefaultDependencies=no"), "{unit}");
    for s in &settings {
        assert!(!s.starts_with("PrivateTmp="), "{s}");
        assert!(!s.starts_with("After=") || !s.contains("local-fs"), "{s}");
    }
    assert!(settings.contains(&"TemporaryFileSystem=/tmp"), "{unit}");
}

fn contrib(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib").join(path)).unwrap()
}

/// The settings of a unit, comments and continuation lines joined.
fn settings(unit: &str) -> Vec<String> {
    unit.replace("\\\n", " ")
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .map(String::from)
        .collect()
}

/// A run of the attach units ends before systemd's start timeout kills it:
/// udev settle, then spaces attach within SPACES_ATTACH_TIMEOUT, which
/// bounds its own waits (udev, servers, the checks). 1.2.2 had 30 s of
/// settle, up to 60 s more of it in the program and 30 s per server
/// against 120 s.
#[test]
fn the_attach_units_end_within_their_start_timeout() {
    for name in ["storage-spaces-attach.service", "storage-spaces-attach@.service"] {
        let s = settings(&contrib(&format!("systemd/{name}")));
        let value = |key: &str| -> Option<u64> {
            s.iter()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.trim().parse().ok())
        };
        let start = value("TimeoutStartSec=").unwrap();
        let settle = s
            .iter()
            .find_map(|l| l.strip_prefix("ExecStartPre=-/usr/bin/udevadm settle --timeout="))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap();
        let run =
            value("Environment=SPACES_ATTACH_TIMEOUT=").unwrap_or_else(|| panic!("{name}: no SPACES_ATTACH_TIMEOUT"));
        assert!(
            settle + run + 5 <= start,
            "{name}: {settle} + {run} s against {start} s"
        );
    }
}

/// Every member that appears starts a run of its own, an instance of the
/// template named after its device numbers: a start of the one unit while
/// it runs joined that run, which had scanned the disks already, and 1.2.2
/// missed the member. The template is the unit of the run at boot but for
/// its ordering after it, and masking that unit masks it.
#[test]
fn a_member_that_appears_starts_a_run_of_its_own() {
    let rule = contrib("udev/69-storage-spaces.rules").replace("\\\n", "");
    assert!(
        rule.lines().any(|l| !l.starts_with('#')
            && l.contains("ENV{ID_PART_ENTRY_TYPE}==\"e75caf8f-f680-4cee-afa3-b001e56efc2d\"")
            && l.contains("ENV{SYSTEMD_WANTS}+=\"storage-spaces-attach@$env{MAJOR}:$env{MINOR}.service\"")),
        "{rule}"
    );
    let main = settings(&contrib("systemd/storage-spaces-attach.service"));
    let each = settings(&contrib("systemd/storage-spaces-attach@.service"));
    let service = |s: &[String]| -> Vec<String> {
        s.iter()
            .skip_while(|l| !l.starts_with("Type="))
            .filter(|l| !l.starts_with("ExecCondition=") && !l.starts_with("WantedBy="))
            .cloned()
            .collect()
    };
    assert_eq!(service(&each), service(&main));
    assert!(
        each.iter()
            .any(|l| l.starts_with("After=") && l.contains("storage-spaces-attach.service"))
    );
    assert!(
        each.iter()
            .any(|l| l.starts_with("ExecCondition=") && l.contains("masked"))
    );
    assert!(
        !each
            .iter()
            .any(|l| l.starts_with("WantedBy=") || l.starts_with("Before=local-fs-pre"))
    );
}

/// Nothing of the attach units needs /var, a file system of its own on some
/// machines, which is not mounted yet when they run at boot (1.2.2 had
/// TemporaryFileSystem=/tmp /var/tmp: systemd makes a missing directory on
/// the root file system, read-only that early).
#[test]
fn the_attach_units_need_nothing_of_var() {
    for name in ["storage-spaces-attach.service", "storage-spaces-attach@.service"] {
        for l in settings(&contrib(&format!("systemd/{name}"))) {
            assert!(!l.contains("/var"), "{name}: {l}");
        }
    }
}

/// udev starts the attach unit for every member disk that appears or
/// changes, several times within seconds at boot: under the default start
/// limit (5 in 10 s) a member that came later was not attached.
#[test]
fn the_attach_unit_has_no_start_limit() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/systemd/storage-spaces-attach.service");
    let unit = std::fs::read_to_string(path).unwrap();
    let section = unit.split("[Service]").next().unwrap();
    assert!(section.lines().any(|l| l == "StartLimitIntervalSec=0"), "{unit}");
}

/// The udev rule imports what attach writes for the devices of attached
/// spaces, and hides from udisks2 the devices a space is served through
/// (with their kernel partitions, which duplicate the space's partition
/// devices) and the devices of spaces attached past their verdict.
#[test]
fn the_udev_rule_hides_what_nothing_should_mount_unasked() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/udev/69-storage-spaces.rules");
    let rule = std::fs::read_to_string(path).unwrap().replace("\\\n", "");
    let lines: Vec<&str> = rule
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .collect();
    let has = |parts: &[&str]| lines.iter().any(|l| parts.iter().all(|p| l.contains(p)));
    assert!(
        has(&[
            "KERNEL==\"dm-*\"",
            "ENV{DM_NAME}==\"ss-*\"",
            "IMPORT{file}=\"/run/storage-spaces/udev/$env{DM_NAME}.props\""
        ]),
        "{rule}"
    );
    assert!(
        has(&[
            "ENV{DEVTYPE}==\"disk\"",
            "IMPORT{file}=\"/run/storage-spaces/udev/$kernel.props\""
        ]),
        "{rule}"
    );
    assert!(
        has(&[
            "ENV{DEVTYPE}==\"partition\"",
            "IMPORT{file}=\"/run/storage-spaces/udev/$parent.props\""
        ]),
        "{rule}"
    );
    assert!(has(&["ENV{SS_BACKEND}==\"1\"", "ENV{UDISKS_IGNORE}=\"1\""]), "{rule}");
    assert!(
        has(&["ENV{SS_VERDICT}!=\"healthy\"", "ENV{UDISKS_IGNORE}=\"1\""]),
        "{rule}"
    );
    // The attach on member disks stays, but not for the devices of attached
    // spaces: the line comes after their properties are imported.
    assert!(
        has(&[
            "e75caf8f-f680-4cee-afa3-b001e56efc2d",
            "ENV{SS_SPACE}!=\"?*\"",
            "SYSTEMD_WANTS"
        ]),
        "{rule}"
    );
    let at = |part: &str| lines.iter().position(|l| l.contains(part)).unwrap();
    assert!(at("SYSTEMD_WANTS") > at("$parent.props"), "{rule}");
    assert!(at("SYSTEMD_WANTS") < at("LABEL=\"storage_spaces_end\""), "{rule}");
}

/// No kernel module is listed for systemd-modules-load: on a kernel without
/// ublk_drv (before 6.0, or built without it) the service failed at every
/// boot (1.2.2's package and install.sh listed ublk_drv and nbd). `spaces
/// attach` loads a backend's module when it needs one.
#[test]
fn no_module_is_listed_to_load_at_boot() {
    assert!(!contrib("deb/build-deb.sh").contains("modules-load.d"));
    let install = contrib("install.sh");
    assert!(
        install
            .lines()
            .filter(|l| l.contains("modules-load.d") && !l.trim_start().starts_with('#'))
            .all(|l| l.contains("rm -f") || l.contains("cat ")),
        "install.sh writes a module list"
    );
    assert!(!contrib("arch/PKGBUILD").contains("modules-load.d"));
}
