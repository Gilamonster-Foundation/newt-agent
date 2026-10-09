//! #2831: compare index and HEAD via local object reads, never Git subprocesses.
use super::*;

pub(super) fn index_is_head(root: &Path, read: &crate::Scope<String>) -> bool {
    head_matches(root, read).unwrap_or(false)
}

fn head_matches(root: &Path, read: &crate::Scope<String>) -> Option<bool> {
    let (admin, _, index) = index(root, read)?;
    let common = grit_lib::refs::common_dir(&admin).unwrap_or_else(|| admin.clone());
    let held =
        crate::git_staging::HeldRoots::bind(&crate::Scope::only([common.to_str()?.to_owned()]))
            .ok()?;
    let branch = crate::git_staging::read_head_branch(
        &admin,
        &crate::git_staging::HeldRoots::bind(read).ok()?,
    )
    .ok()?;
    let oid =
        ObjectId::from_hex(&crate::git_staging::read_branch_oid(&common, &branch, &held).ok()?)
            .ok()?;
    // Odb::read reads loose/packed objects and local alternates only. Unlike
    // native Git it has no lazy-fetch child, replacement-object or filter path.
    let odb = grit_lib::odb::Odb::new(&common.join("objects"));
    let object = read_object(&odb, &oid, ObjectKind::Commit)?;
    let commit = grit_lib::objects::parse_commit(&object).ok()?;
    let mut queue = vec![(Vec::new(), commit.tree)];
    let mut tree = BTreeMap::new();
    let mut visited = 0usize;
    while let Some((prefix, oid)) = queue.pop() {
        visited += 1;
        if visited > MAX_ENTRIES || tree.len() > MAX_ENTRIES {
            return None;
        }
        let bytes = read_object(&odb, &oid, ObjectKind::Tree)?;
        for entry in
            grit_lib::objects::parse_tree_with_oid_len(&bytes, index.hash_algo.len()).ok()?
        {
            let mut path = prefix.clone();
            path.extend_from_slice(&entry.name);
            match entry.mode {
                0o40000 => {
                    path.push(b'/');
                    queue.push((path, entry.oid));
                }
                0o100644 | 0o100755 => {
                    if tree
                        .insert(path, (entry.mode, entry.oid.as_bytes().to_vec()))
                        .is_some()
                    {
                        return None;
                    }
                }
                _ => return None,
            }
        }
    }
    let staged: BTreeMap<_, _> = index
        .entries
        .iter()
        .map(|e| (e.path.clone(), (e.mode, e.oid.as_bytes().to_vec())))
        .collect();
    Some(tree == staged)
}

fn read_object(odb: &grit_lib::odb::Odb, oid: &ObjectId, kind: ObjectKind) -> Option<Vec<u8>> {
    let object = odb.read(oid).ok()?;
    if object.kind != kind
        || object.data.len() > MAX_BYTES
        || grit_lib::pack::hash_object_bytes(kind, &object.data, oid.as_bytes().len()).ok()?
            != oid.as_bytes()
    {
        return None;
    }
    Some(object.data)
}
