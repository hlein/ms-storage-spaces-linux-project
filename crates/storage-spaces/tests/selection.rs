//! Choosing a space by what people type: its GUID, its numeric id or its
//! name, one way for every command (attach, check, info, export, the
//! servers, refs). A key that names two spaces (a name that is another
//! space's GUID or id, a name two spaces share) is refused with both
//! named. 1.2.2 took the first space named or numbered so, and `spaces
//! check` the last: a space named like another space's GUID was read and
//! checked in its place (audit 2026-10-16, M12).

use std::fs::File;
use std::path::Path;

use storage_spaces::Pool;
use storage_spaces::io::SparseImage;

fn load(name: &str) -> Pool<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let disks: Vec<SparseImage> = (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect();
    Pool::open(disks).unwrap()
}

#[test]
fn a_key_that_names_two_spaces_is_refused() {
    let mut pool = load("multi");
    let ids: Vec<u64> = pool.user_spaces().map(|s| s.id()).collect();
    assert_eq!(ids.len(), 3);
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    let guid = |pool: &Pool<SparseImage>, id: u64| pool.spaces[&id].info.guid.to_string();
    let (guid_a, guid_c) = (guid(&pool, a), guid(&pool, c));
    // Each by its GUID (any case), id and name.
    for id in ids.iter().copied() {
        let name = pool.spaces[&id].name().to_string();
        for key in [guid(&pool, id), guid(&pool, id).to_uppercase(), id.to_string(), name] {
            assert_eq!(pool.select_space(&key).unwrap().map(|s| s.id()), Some(id), "{key}");
        }
    }
    assert!(pool.select_space("nothing").unwrap().is_none());

    // The first space named like the third's GUID: neither is taken.
    pool.spaces.get_mut(&a).unwrap().info.name = guid_c.clone();
    assert!(pool.find_space(&guid_c).is_none());
    let e = pool.select_space(&guid_c).unwrap_err().to_string();
    assert!(e.contains(&guid_a) && e.contains(&guid_c), "{e}");
    // By its own GUID the first is still found.
    assert_eq!(pool.select_space(&guid_a).unwrap().map(|s| s.id()), Some(a));

    // A name two spaces share, and a name that is another space's id.
    let name_c = pool.spaces[&c].name().to_string();
    pool.spaces.get_mut(&b).unwrap().info.name = name_c.clone();
    assert!(pool.select_space(&name_c).is_err());
    pool.spaces.get_mut(&b).unwrap().info.name = c.to_string();
    assert!(pool.select_space(&c.to_string()).is_err());
}
