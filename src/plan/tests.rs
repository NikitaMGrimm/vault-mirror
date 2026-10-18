use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::json;

use super::*;
use crate::vault::{Attachment, Collection, Folder};

fn text(value: &str) -> Node {
    Node::secret(value)
}

fn map(entries: Vec<(&str, Node)>) -> Node {
    Node::Map(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn login(id: &str, name: &str, pw: &str) -> Item {
    let content = BTreeMap::from([
        ("type".to_string(), Node::Number(1.into())),
        ("name".to_string(), text(name)),
        ("notes".to_string(), Node::Null),
        ("favorite".to_string(), Node::Bool(false)),
        ("reprompt".to_string(), Node::Number(0.into())),
        (
            "login".to_string(),
            map(vec![
                ("username", text("user")),
                ("password", text(pw)),
                ("totp", Node::Null),
            ]),
        ),
        ("fields".to_string(), Node::List(vec![])),
    ]);
    Item {
        id: id.to_string(),
        folder_id: None,
        collection_ids: vec![],
        revision_date: Some("2026-01-01T00:00:00.000000Z".into()),
        creation_date: Some("2026-01-01T00:00:00.000000Z".into()),
        in_trash: false,
        archived: false,
        archived_date: None,
        content,
        attachments: vec![],
        wrapped_key: None,
    }
}

fn attachment(id: &str, name: &str, size: u64) -> Attachment {
    Attachment {
        id: id.to_string(),
        file_name: name.to_string(),
        size,
        wrapped_key: None,
    }
}

fn folder(id: &str, name: &str) -> Folder {
    Folder {
        id: id.to_string(),
        name: name.to_string(),
        stamp: None,
    }
}

fn collection(id: &str, name: &str) -> Collection {
    Collection {
        id: id.to_string(),
        name: name.to_string(),
        external_id: None,
    }
}

fn vault(items: Vec<Item>) -> Vault {
    Vault {
        items,
        ..Vault::default()
    }
}

const PAIR: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const OTHER_PAIR: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn plan(source: &Vault, dest: &Vault, mode: Mode) -> Plan {
    super::plan(source, dest, mode, PAIR)
}

fn marker(item: &Item) -> Option<Marker> {
    marker_of(item, PAIR)
}

fn set_marker(item: &mut Item, marker: Marker) {
    let mut content = unmarked(item);
    let Some(Node::List(fields)) = content.get_mut("fields") else {
        panic!()
    };
    fields.push(marker.to_field());
    item.content = content;
}

fn check(plan: &Plan, dest: &Vault, safety: &Safety) -> Result<(), SafetyViolation> {
    check_safety(plan, dest, safety)
}

/// Applies a plan as a run that finishes does, without a server: the
/// destination ends up holding exactly what the final drafts describe.
fn apply(source: &Vault, dest: &Vault, mode: Mode) -> Vault {
    let plan = plan(source, dest, mode);
    let mut next = dest.clone();
    let taken: HashSet<String> = (dest.items.iter().map(|i| i.id.clone()))
        .chain(dest.folders.iter().map(|f| f.id.clone()))
        .chain(dest.collections.iter().map(|c| c.id.clone()))
        .chain(
            dest.items
                .iter()
                .flat_map(|i| i.attachments.iter().map(|a| a.id.clone())),
        )
        .collect();
    let mut counter = 0;
    let mut fresh = |prefix: &str| loop {
        counter += 1;
        let id = format!("{prefix}-{counter}");
        if !taken.contains(&id) {
            break id;
        }
    };

    let mut folder_ids = plan.folder_ids.clone();
    for name in &plan.folders_to_create {
        let id = fresh("dest-folder");
        next.folders.push(folder(&id, name));
        folder_ids.insert(name.clone(), id);
    }
    let mut collection_ids = plan.collection_ids.clone();
    for new in &plan.collections_to_create {
        let id = fresh("dest-collection");
        next.collections.push(collection(&id, &new.name));
        collection_ids.insert(new.name.clone(), id);
    }

    let write = |item: &mut Item, entry: &Entry, files: Vec<Attachment>| {
        let archived = source.items[entry.source].archived;
        let draft = entry
            .draft(source, &folder_ids, &collection_ids, archived, Some(&files))
            .unwrap();
        item.content = draft.content;
        item.folder_id = draft.folder_id;
        item.collection_ids = draft.collection_ids;
        item.archived = archived;
        item.attachments = files;
    };
    let copies = |entry: &Entry, fresh: &mut dyn FnMut(&str) -> String| -> Vec<Attachment> {
        (source.items[entry.source].attachments.iter())
            .map(|a| attachment(&fresh("dest-attachment"), &a.file_name, a.size))
            .collect()
    };
    for &entry_index in &plan.creates {
        let entry = &plan.entries[entry_index];
        let mut item = login(&fresh("dest-item"), "", "");
        let files = copies(entry, &mut fresh);
        write(&mut item, entry, files);
        next.items.push(item);
    }
    for update in &plan.updates {
        let entry = &plan.entries[update.entry];
        let files = match update.sync_attachments {
            true => copies(entry, &mut fresh),
            false => next.items[update.dest].attachments.clone(),
        };
        write(&mut next.items[update.dest], entry, files);
    }
    let doomed: Vec<String> = plan
        .deletes
        .iter()
        .map(|d| dest.items[d.dest].id.clone())
        .collect();
    next.items.retain(|item| !doomed.contains(&item.id));
    next
}

/// A copy of `source`'s first item as a run leaves it that stopped before
/// copying any attachment.
fn pending(source: &Vault, copy: &Item) -> Item {
    let entry = &plan(source, &Vault::default(), Mode::Personal).entries[0];
    let draft = (entry.draft(source, &HashMap::new(), &HashMap::new(), false, None)).unwrap();
    Item {
        content: draft.content,
        attachments: vec![],
        ..copy.clone()
    }
}

fn assert_converged(source: &Vault, dest: &Vault, mode: Mode) {
    let again = plan(source, dest, mode);
    assert!(!again.has_changes(), "plan still has changes: {again:#?}");
    let live = source.items.iter().filter(|item| !item.in_trash).count();
    assert_eq!(again.unchanged, live);
    let copies = dest
        .items
        .iter()
        .filter(|item| !item.in_trash && marker(item).is_some())
        .count();
    assert_eq!(copies, live);
}

fn mirrored(source: &Vault, mode: Mode) -> Vault {
    let dest = apply(source, &Vault::default(), mode);
    assert_converged(source, &dest, mode);
    dest
}

#[test]
fn an_empty_destination_gets_every_item_and_folder() {
    let mut source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    source.folders = vec![folder("f1", "Work"), folder("f2", "Empty")];
    source.items[0].folder_id = Some("f1".into());

    let plan = plan(&source, &Vault::default(), Mode::Personal);
    assert_eq!(plan.creates, [0, 1]);
    assert_eq!(plan.folders_to_create, ["Empty", "Work"]);
    assert!(plan.updates.is_empty() && plan.deletes.is_empty());
    assert_eq!(plan.summary(&source).items_created, 2);

    let dest = mirrored(&source, Mode::Personal);
    assert_eq!(dest.folders.len(), 2);
}

#[test]
fn the_marker_is_the_last_field_and_names_the_vaults_and_the_source_item() {
    let mut source = vault(vec![login("s1", "One", "pw")]);
    source.items[0].content.insert(
        "fields".into(),
        Node::List(vec![map(vec![
            ("name", text("pin")),
            ("value", text("1234")),
        ])]),
    );
    let dest = mirrored(&source, Mode::Personal);

    let fields = dest.items[0].fields();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].as_map().unwrap()["name"], text("pin"));
    assert!(is_mirror_marker(&fields[1]));
    assert_eq!(fields[1].as_map().unwrap()["type"], Node::Number(1.into()));
    let marker = marker(&dest.items[0]).unwrap();
    assert_eq!(
        (marker.pair.as_str(), marker.source_id.as_str()),
        (PAIR, "s1")
    );
    assert_eq!(
        (
            marker.archived,
            marker.source_files.as_str(),
            marker.dest_files.as_str()
        ),
        (false, "-", "-")
    );
}

#[test]
fn a_pair_is_the_same_in_either_direction() {
    assert_eq!(pair("a", "b"), pair("b", "a"));
    assert_ne!(pair("a", "b"), pair("a", "c"));
    assert!(is_hash(&pair("a", "b")));
}

#[test]
fn a_source_edit_rewrites_only_that_item() {
    let mut source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    let dest = mirrored(&source, Mode::Personal);
    source.items[1] = login("s2", "Two", "changed");

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.unchanged, 1);
    assert_eq!(
        plan.updates,
        [Update {
            entry: 1,
            dest: 1,
            write_content: true,
            sync_attachments: false,
            set_archived: None,
            changed_in_dest: Changes::default(),
        }]
    );
    assert!(plan.creates.is_empty() && plan.deletes.is_empty());
    assert_eq!(check(&plan, &dest, &Safety::default()), Ok(()));

    let dest = apply(&source, &dest, Mode::Personal);
    assert_converged(&source, &dest, Mode::Personal);
    // Updated in place: the destination item keeps its identity.
    assert_eq!(dest.items[1].id, "dest-item-2");
}

#[test]
fn a_copy_edited_in_the_destination_is_replaced_only_when_allowed() {
    let source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    let mut dest = mirrored(&source, Mode::Personal);
    let Some(Node::Map(login)) = dest.items[0].content.get_mut("login") else {
        panic!()
    };
    login.insert("password".into(), text("tampered"));
    dest.folders.push(folder("mine", "Mine"));
    dest.items[1].folder_id = Some("mine".into());

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.updates.len(), 2);
    assert!(plan.updates.iter().all(|u| u.write_content));
    assert!(plan.updates.iter().all(|u| u.changed_in_dest.content));
    assert_eq!(
        check(&plan, &dest, &Safety::default()),
        Err(SafetyViolation::Overwrites(vec![
            "dest-item-1".into(),
            "dest-item-2".into()
        ]))
    );
    let overwrite = Safety {
        allow_overwrite: true,
        ..Safety::default()
    };
    assert_eq!(check(&plan, &dest, &overwrite), Ok(()));
    assert_converged(
        &source,
        &apply(&source, &dest, Mode::Personal),
        Mode::Personal,
    );
}

#[test]
fn the_same_change_on_both_sides_overwrites_nothing() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    let mut dest = mirrored(&source, Mode::Personal);
    for item in [&mut source.items[0], &mut dest.items[0]] {
        item.content.insert("notes".into(), text("the same edit"));
    }
    let plan = plan(&source, &dest, Mode::Personal);
    assert!(!plan.updates[0].changed_in_dest.any());
    assert_eq!(check(&plan, &dest, &Safety::default()), Ok(()));
}

#[test]
fn copies_without_a_live_source_are_deleted_with_a_reason() {
    let source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    let mut dest = mirrored(&source, Mode::Personal);
    let source = vault(vec![login("s1", "One", "pw1")]);
    dest.items.push(login("manual", "Added by hand", "x"));
    let mut trashed = dest.items[0].clone();
    trashed.id = "in-trash".into();
    trashed.in_trash = true;
    dest.items.push(trashed);
    let mut duplicate = dest.items[0].clone();
    duplicate.id = "copy".into();
    dest.items.push(duplicate);

    let plan = plan(&source, &dest, Mode::Personal);
    let reasons: Vec<(&str, DeleteReason)> = plan
        .deletes
        .iter()
        .map(|d| (dest.items[d.dest].id.as_str(), d.reason))
        .collect();
    assert_eq!(
        reasons,
        [
            ("dest-item-2", DeleteReason::Stale),
            ("copy", DeleteReason::Duplicate),
        ]
    );
    assert_eq!(plan.unchanged, 1);
    assert_eq!(plan.untracked, 1, "the item added by hand stays");
    assert_converged(
        &source,
        &apply(&source, &dest, Mode::Personal),
        Mode::Personal,
    );
}

#[test]
fn a_malformed_marker_is_not_trusted() {
    let source = vault(vec![login("s1", "One", "pw1")]);
    let mut dest = mirrored(&source, Mode::Personal);
    let Some(Node::List(fields)) = dest.items[0].content.get_mut("fields") else {
        panic!()
    };
    fields[0] = map(vec![
        ("name", text(MARKER_FIELD)),
        ("value", text("only-one-part")),
    ]);
    assert_eq!(marker(&dest.items[0]), None);
    let without_attachments = format!("s1 {}", "0123abcd".repeat(8));
    let Some(Node::List(fields)) = dest.items[0].content.get_mut("fields") else {
        panic!()
    };
    fields[0] = map(vec![
        ("name", text(MARKER_FIELD)),
        ("value", text(&without_attachments)),
    ]);

    assert_eq!(marker(&dest.items[0]), None);
    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.creates, [0]);
    assert!(plan.deletes.is_empty());
    assert_eq!(plan.untracked, 1);
}

#[test]
fn a_field_that_only_looks_like_a_marker_is_not_trusted() {
    let source = vault(vec![login("s1", "One", "pw1")]);
    let mut dest = vault(vec![login("d1", "Someone's item", "pw")]);
    let Some(Node::List(fields)) = dest.items[0].content.get_mut("fields") else {
        panic!()
    };
    fields.push(map(vec![
        ("name", text(MARKER_FIELD)),
        ("value", text("not-a-source-id not-a-hash")),
    ]));

    assert_eq!(marker(&dest.items[0]), None);
    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!((plan.untracked, plan.deletes.len()), (1, 0));
}

#[test]
fn items_in_the_source_trash_are_not_mirrored() {
    let mut source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    let dest = mirrored(&source, Mode::Personal);
    source.items[1].in_trash = true;

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(
        plan.deletes,
        [Delete {
            dest: 1,
            reason: DeleteReason::Stale,
            changed_in_dest: Changes::default(),
        }]
    );
    assert_converged(
        &source,
        &apply(&source, &dest, Mode::Personal),
        Mode::Personal,
    );
}

#[test]
fn unowned_folders_and_collections_are_left_alone() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.folders = vec![folder("f1", "Old")];
    source.items[0].folder_id = Some("f1".into());
    let mut dest = mirrored(&source, Mode::Personal);
    dest.folders.push(folder("mine", "Empty and mine"));
    let emptied = apply(
        &vault(vec![login("s2", "Two", "pw2")]),
        &dest,
        Mode::Personal,
    );
    assert_eq!(emptied.folders, dest.folders);

    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.collections = vec![collection("c1", "Team")];
    source.items[0].collection_ids = vec!["c1".into()];
    let dest = mirrored(&source, Mode::Organization);
    let emptied = apply(&vault(vec![]), &dest, Mode::Organization);
    assert_eq!(emptied.collections, dest.collections);
}

#[test]
fn a_copy_changed_after_its_source_item_went_is_told_apart() {
    let mut source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    source.items[1].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let dest = mirrored(&source, Mode::Personal);
    source.items[1].in_trash = true;
    let changed = |dest: &Vault| plan(&source, dest, Mode::Personal).deletes[0].changed_in_dest;
    assert!(!changed(&dest).any());

    let mut edited = dest.clone();
    edited.items[1]
        .content
        .insert("notes".into(), text("edited"));
    assert!(changed(&edited).content);
    let mut refiled = dest.clone();
    refiled.items[1].attachments = vec![attachment("a2", "scan.pdf", 100)];
    assert!(changed(&refiled).files);
    let mut archived = dest.clone();
    archived.items[1].archived = true;
    assert!(changed(&archived).archive);
    let plan = plan(&source, &archived, Mode::Personal);
    assert_eq!(
        check(&plan, &archived, &Safety::default()),
        Err(SafetyViolation::Overwrites(vec!["dest-item-2".into()]))
    );
}

#[test]
fn attachments_are_copied_again_only_when_they_changed() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let dest = mirrored(&source, Mode::Personal);
    assert_eq!(
        plan(&source, &Vault::default(), Mode::Personal)
            .summary(&source)
            .attachments_copied,
        1
    );

    // A content change leaves the attachment alone.
    let mut edited = source.clone();
    edited.items[0]
        .content
        .insert("name".into(), text("Renamed"));
    let update = &plan(&edited, &dest, Mode::Personal).updates[0];
    assert!(update.write_content && !update.sync_attachments);

    // A file replaced under the same name and size gets a new attachment id.
    let mut replaced = source.clone();
    replaced.items[0].attachments = vec![attachment("a2", "scan.pdf", 100)];
    let update = &plan(&replaced, &dest, Mode::Personal).updates[0];
    assert!(update.sync_attachments);
    assert!(
        update.write_content,
        "the marker has to record the new attachment"
    );
    assert_converged(
        &replaced,
        &apply(&replaced, &dest, Mode::Personal),
        Mode::Personal,
    );

    // An attachment removed from the destination by hand is a change there.
    let mut damaged = dest.clone();
    damaged.items[0].attachments.clear();
    let update = &plan(&source, &damaged, Mode::Personal).updates[0];
    assert!(update.sync_attachments && update.changed_in_dest.files);

    // Removing the last source attachment removes it from the destination.
    let mut removed = source.clone();
    removed.items[0].attachments.clear();
    let after = apply(&removed, &dest, Mode::Personal);
    assert!(after.items[0].attachments.is_empty());
    assert_converged(&removed, &after, Mode::Personal);
}

#[test]
fn a_copy_left_before_its_first_attachment_gets_them_all() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let mut dest = mirrored(&source, Mode::Personal);
    dest.items[0] = pending(&source, &dest.items[0]);

    let plan = plan(&source, &dest, Mode::Personal);
    let update = &plan.updates[0];
    assert!(update.sync_attachments && !update.changed_in_dest.any());
    assert_eq!(check(&plan, &dest, &Safety::default()), Ok(()));
    assert_converged(
        &source,
        &apply(&source, &dest, Mode::Personal),
        Mode::Personal,
    );
}

/// Nothing tells files a stopped run uploaded from files someone added
/// since, so they are kept until the user allows the run to replace them.
#[test]
fn attachments_no_marker_vouches_for_are_not_replaced_unasked() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let mut dest = mirrored(&source, Mode::Personal);
    dest.items[0] = Item {
        attachments: vec![attachment("d1", "scan.pdf", 164)],
        ..pending(&source, &dest.items[0])
    };

    let plan = plan(&source, &dest, Mode::Personal);
    let update = &plan.updates[0];
    assert!(update.sync_attachments && update.changed_in_dest.files);
    assert_eq!(
        check(&plan, &dest, &Safety::default()),
        Err(SafetyViolation::Overwrites(vec!["dest-item-1".into()]))
    );

    // With its source item gone, the copy is not deleted unasked either.
    let plan = super::plan(&vault(vec![]), &dest, Mode::Personal, PAIR);
    assert!(plan.deletes[0].changed_in_dest.files);
}

#[test]
fn a_destination_file_replaced_under_the_same_name_is_a_change_there() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let mut dest = mirrored(&source, Mode::Personal);
    dest.items[0].attachments = vec![attachment("replaced", "scan.pdf", 100)];
    let plan = plan(&source, &dest, Mode::Personal);
    assert!(plan.updates[0].sync_attachments && plan.updates[0].changed_in_dest.files);
    assert_converged(
        &source,
        &apply(&source, &dest, Mode::Personal),
        Mode::Personal,
    );
}

#[test]
fn destination_attachment_renames_require_permission() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let dest = mirrored(&source, Mode::Personal);

    let mut edited = dest.clone();
    edited.items[0].attachments[0].file_name = "renamed.pdf".into();
    let expected = Err(SafetyViolation::Overwrites(vec![dest.items[0].id.clone()]));

    let planned = plan(&source, &edited, Mode::Personal);
    assert_eq!(planned.updates.len(), 1);
    assert!(planned.updates[0].changed_in_dest.files);
    assert_eq!(check(&planned, &edited, &Safety::default()), expected);

    let stale = plan(&vault(vec![]), &edited, Mode::Personal);
    assert_eq!(stale.deletes.len(), 1);
    assert!(stale.deletes[0].changed_in_dest.files);
    assert_eq!(check(&stale, &edited, &Safety::default()), expected);

    edited.items[0].id = "edited-clone".into();
    for clone_first in [false, true] {
        let mut duplicated = dest.clone();
        duplicated
            .items
            .insert(usize::from(!clone_first), edited.items[0].clone());
        let planned = plan(&source, &duplicated, Mode::Personal);
        assert!(planned.deletes.is_empty());
        assert_eq!(planned.detached, 1);
    }
}

#[test]
fn attachment_sizes_on_the_destination_do_not_matter() {
    // Sizes are ciphertext sizes and depend on the encryption format in use.
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let mut dest = mirrored(&source, Mode::Personal);
    dest.items[0].attachments[0].size = 164;
    assert_converged(&source, &dest, Mode::Personal);
}

#[test]
fn archive_state_is_mirrored_and_recorded_in_the_marker() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    let dest = mirrored(&source, Mode::Personal);
    source.items[0].archived = true;

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(
        plan.updates,
        [Update {
            entry: 0,
            dest: 0,
            write_content: true,
            sync_attachments: false,
            set_archived: Some(true),
            changed_in_dest: Changes::default(),
        }]
    );
    let after = apply(&source, &dest, Mode::Personal);
    assert!(marker(&after.items[0]).unwrap().archived);
    assert_converged(&source, &after, Mode::Personal);
}

#[test]
fn an_archive_change_in_the_destination_is_a_change_there() {
    let source = vault(vec![login("s1", "One", "pw1")]);
    let mut dest = mirrored(&source, Mode::Personal);
    dest.items[0].archived = true;
    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.updates[0].set_archived, Some(false));
    assert!(plan.updates[0].changed_in_dest.archive);
}

/// A run writes each copy's marker for the archive state it is in, sets the
/// state, then updates the marker. Stopped in between, it left no change
/// for the next run to refuse.
#[test]
fn a_run_stopped_while_archiving_left_no_change_in_the_destination() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].archived = true;
    let entry = &plan(&source, &Vault::default(), Mode::Personal).entries[0];
    let written = |archived: bool| {
        let draft =
            (entry.draft(&source, &HashMap::new(), &HashMap::new(), false, Some(&[]))).unwrap();
        Item {
            content: draft.content,
            archived,
            ..login("copy", "", "")
        }
    };
    for archived in [false, true] {
        let dest = vault(vec![written(archived)]);
        let plan = plan(&source, &dest, Mode::Personal);
        assert!(!plan.updates[0].changed_in_dest.any(), "{archived}");
        assert!(plan.updates[0].write_content, "{archived}");
        assert_converged(
            &source,
            &apply(&source, &dest, Mode::Personal),
            Mode::Personal,
        );
    }
}

#[test]
fn a_renamed_folder_moves_its_items_and_stays() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.folders = vec![folder("f1", "Old")];
    source.items[0].folder_id = Some("f1".into());
    let dest = mirrored(&source, Mode::Personal);
    source.folders[0].name = "New".into();

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.folders_to_create, ["New"]);
    assert_eq!(plan.updates.len(), 1);
    assert!(!plan.updates[0].changed_in_dest.any());
    let after = apply(&source, &dest, Mode::Personal);
    assert_converged(&source, &after, Mode::Personal);
    assert_eq!(after.folders.len(), 2);
    assert_eq!(
        after.items[0].folder_id.as_deref(),
        Some(after.folders[1].id.as_str())
    );
}

#[test]
fn folders_sharing_a_name_are_one_place() {
    let mut source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    source.folders = vec![folder("f1", "Same"), folder("f2", "Same")];
    source.items[0].folder_id = Some("f1".into());
    source.items[1].folder_id = Some("f2".into());
    let mut dest = mirrored(&source, Mode::Personal);
    assert_eq!(dest.folders.len(), 1);

    // A copy moved into a second folder of the name is where it belongs.
    dest.folders.push(folder("dup", "Same"));
    dest.items[1].folder_id = Some("dup".into());
    assert_converged(&source, &dest, Mode::Personal);
}

#[test]
fn a_dangling_folder_reference_means_no_folder() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].folder_id = Some("gone".into());
    let dest = mirrored(&source, Mode::Personal);
    assert_eq!(dest.items[0].folder_id, None);
}

#[test]
fn organization_items_follow_collections_by_name() {
    let mut source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    source.collections = vec![
        collection("c1", "Team A"),
        collection("c2", "Team B"),
        collection("c3", "Empty"),
    ];
    source.collections[1].external_id = Some("ext-b".into());
    source.items[0].collection_ids = vec!["c2".into(), "c1".into()];
    source.items[1].collection_ids = vec!["c1".into()];
    // Folders are a per-user view and are not part of an organization mirror.
    source.folders = vec![folder("f1", "Mine")];
    source.items[0].folder_id = Some("f1".into());

    let first = plan(&source, &Vault::default(), Mode::Organization);
    assert!(first.folders_to_create.is_empty());
    let names: Vec<&str> = first
        .collections_to_create
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["Empty", "Team A", "Team B"]);
    assert_eq!(
        first.collections_to_create[2].external_id.as_deref(),
        Some("ext-b")
    );

    let dest = mirrored(&source, Mode::Organization);
    assert_eq!(dest.items[0].folder_id, None);
    assert_eq!(dest.items[0].collection_ids.len(), 2);
    assert!(
        dest.items[0]
            .collection_ids
            .iter()
            .all(|id| id.starts_with("dest-collection"))
    );

    // Moving an item between collections is a change; reordering ids is not.
    let mut reordered = source.clone();
    reordered.items[0].collection_ids.reverse();
    assert_converged(&reordered, &dest, Mode::Organization);
    let mut moved = source.clone();
    moved.items[1].collection_ids = vec!["c2".into()];
    moved.collections.pop();
    let plan = plan(&moved, &dest, Mode::Organization);
    assert_eq!(plan.updates.len(), 1);
    assert_converged(
        &moved,
        &apply(&moved, &dest, Mode::Organization),
        Mode::Organization,
    );
}

#[test]
fn server_echo_differences_are_not_changes() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    let Some(Node::Map(source_login)) = source.items[0].content.get_mut("login") else {
        panic!()
    };
    source_login.insert(
        "uris".into(),
        Node::List(vec![map(vec![
            ("uri", text("https://example.com")),
            ("match", Node::Null),
        ])]),
    );
    source_login.insert(
        "passwordRevisionDate".into(),
        Node::Text("2026-03-04T05:06:07.1234567Z".into()),
    );
    let mut dest = mirrored(&source, Mode::Personal);

    let content = &mut dest.items[0].content;
    // Fields that are null on one server are missing or empty on another.
    content.remove("notes");
    content.insert("passwordHistory".into(), Node::List(vec![]));
    content.insert("card".into(), Node::Null);
    content.insert("secureNote".into(), map(vec![("type", Node::Null)]));
    let Some(Node::Map(dest_login)) = content.get_mut("login") else {
        panic!()
    };
    dest_login.remove("totp");
    dest_login.insert("fido2Credentials".into(), Node::Null);
    // The first URI is echoed for old clients.
    dest_login.insert("uri".into(), text("https://example.com"));
    // Timestamps come back in another precision.
    dest_login.insert(
        "passwordRevisionDate".into(),
        Node::Text("2026-03-04T05:06:07.123Z".into()),
    );
    dest.items[0].revision_date = Some("2030-01-01T00:00:00Z".into());

    assert_converged(&source, &dest, Mode::Personal);
}

#[test]
fn empty_notes_equal_no_notes_but_a_real_change_is_seen() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].content.insert("notes".into(), text(""));
    let dest = mirrored(&source, Mode::Personal);
    source.items[0].content.insert("notes".into(), Node::Null);
    assert_converged(&source, &dest, Mode::Personal);
    source.items[0]
        .content
        .insert("notes".into(), text("now with text"));
    assert_eq!(plan(&source, &dest, Mode::Personal).updates.len(), 1);
}

#[test]
fn item_types_this_tool_has_never_heard_of_are_mirrored() {
    let mut source = vault(vec![login("s1", "Passport", "")]);
    let content = &mut source.items[0].content;
    content.insert("type".into(), Node::Number(8.into()));
    content.remove("login");
    content.insert(
        "passport".into(),
        map(vec![
            ("number", text("X1234567")),
            ("issued", Node::Text("2026-01-01T00:00:00Z".into())),
        ]),
    );
    let dest = mirrored(&source, Mode::Personal);
    assert_eq!(
        dest.items[0].content["passport"].as_map().unwrap()["number"],
        text("X1234567")
    );

    let Some(Node::Map(passport)) = source.items[0].content.get_mut("passport") else {
        panic!()
    };
    passport.insert("number".into(), text("Y7654321"));
    assert_eq!(plan(&source, &dest, Mode::Personal).updates.len(), 1);
}

#[test]
fn a_draft_without_its_folder_fails_without_naming_it() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.folders = vec![folder("f1", "Work")];
    source.items[0].folder_id = Some("f1".into());
    let plan = plan(&source, &Vault::default(), Mode::Personal);
    let error = plan.entries[0]
        .draft(&source, &HashMap::new(), &HashMap::new(), false, Some(&[]))
        .unwrap_err();
    assert_eq!(error, PlanError::MissingFolder);
    assert!(!error.to_string().contains("Work"));
}

fn numbered(count: usize) -> Vault {
    vault(
        (0..count)
            .map(|i| login(&format!("s{i}"), &format!("Item {i}"), "pw"))
            .collect(),
    )
}

#[test]
fn switching_source_and_destination_takes_over_the_copies() {
    let mut original = numbered(3);
    let mirror = mirrored(&original, Mode::Personal);
    // Changed in the old source after its last run, then the roles switch.
    original.items[0]
        .content
        .insert("notes".into(), text("late edit"));
    let plan = plan(&mirror, &original, Mode::Personal);
    assert_eq!(plan.reversed, 3);
    assert_eq!((plan.creates.len(), plan.deletes.len()), (0, 0));
    assert_eq!(
        check(&plan, &original, &Safety::default()),
        Err(SafetyViolation::Reversed(3))
    );
    let reverse = Safety {
        allow_reverse: true,
        ..Safety::default()
    };
    assert_eq!(
        check(&plan, &original, &reverse),
        Err(SafetyViolation::Overwrites(vec!["s0".into()]))
    );
    let changed: Vec<bool> = (plan.updates.iter())
        .map(|u| u.changed_in_dest.any())
        .collect();
    assert_eq!(changed, [true, false, false]);

    let original = apply(&mirror, &original, Mode::Personal);
    assert_converged(&mirror, &original, Mode::Personal);
    let again = super::plan(&mirror, &original, Mode::Personal, PAIR);
    assert_eq!(check(&again, &original, &Safety::default()), Ok(()));

    // And back: each item still has one marker, and nothing is duplicated.
    let back = super::plan(&original, &mirror, Mode::Personal, PAIR);
    assert_eq!(back.reversed, 0);
    assert_eq!(check(&back, &mirror, &Safety::default()), Ok(()));
    assert!(!back.has_changes());
    for item in original.items.iter().chain(&mirror.items) {
        let markers = item.fields().iter().filter(|f| is_mirror_marker(f)).count();
        assert_eq!(markers, 1, "{}", item.id);
    }
}

#[test]
fn switching_source_and_destination_copies_only_changed_attachments() {
    let mut original = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    original.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    original.items[1].attachments = vec![attachment("a2", "key.txt", 5)];
    let mut mirror = mirrored(&original, Mode::Personal);
    // Replaced in the old destination, which gives it a new id.
    mirror.items[1].attachments = vec![attachment("b2", "key.txt", 6)];

    let plan = plan(&mirror, &original, Mode::Personal);
    let synced: Vec<bool> = plan.updates.iter().map(|u| u.sync_attachments).collect();
    assert_eq!(synced, [false, true]);
}

#[test]
fn switching_back_after_attachments_changed_copies_them_once() {
    let mut a = vault(vec![login("a1", "One", "pw1")]);
    a.items[0].attachments = vec![attachment("f1", "scan.pdf", 100)];
    let mut b = mirrored(&a, Mode::Personal);
    b.items[0].attachments = vec![attachment("f2", "scan.pdf", 120)];
    let a = apply(&b, &a, Mode::Personal);

    // Each side's markers now disagree about the files, but together they
    // show the files match.
    let back = plan(&a, &b, Mode::Personal);
    assert!(
        back.updates
            .iter()
            .all(|u| !u.sync_attachments && !u.changed_in_dest.any())
    );
    let b = apply(&a, &b, Mode::Personal);
    assert_converged(&a, &b, Mode::Personal);
    assert_converged(&b, &a, Mode::Personal);
}

#[test]
fn switching_back_after_content_changed_uses_the_last_written_baseline() {
    let mut a = vault(vec![login("a1", "One", "pw1")]);
    let mut b = mirrored(&a, Mode::Personal);
    b.items[0].content.insert("notes".into(), text("second"));
    a = apply(&b, &a, Mode::Personal);
    a.items[0].content.insert("notes".into(), text("third"));

    let back = plan(&a, &b, Mode::Personal);
    assert_eq!(check(&back, &b, &Safety::default()), Ok(()));
    assert_eq!(back.updates.len(), 1);
    b = apply(&a, &b, Mode::Personal);
    assert_converged(&a, &b, Mode::Personal);
    assert_eq!(b.items[0].content["notes"], text("third"));

    b.items[0].content.insert("notes".into(), text("second"));
    let changed = plan(&a, &b, Mode::Personal);
    assert!(changed.updates[0].changed_in_dest.content);
    assert!(matches!(
        check(&changed, &b, &Safety::default()),
        Err(SafetyViolation::Overwrites(_))
    ));
}

#[test]
fn switching_back_after_archiving_uses_the_last_written_baseline() {
    let mut a = vault(vec![login("a1", "One", "pw1")]);
    let mut b = mirrored(&a, Mode::Personal);
    b.items[0].archived = true;
    a = apply(&b, &a, Mode::Personal);
    a.items[0].archived = false;

    let back = plan(&a, &b, Mode::Personal);
    assert_eq!(check(&back, &b, &Safety::default()), Ok(()));
    assert_eq!(back.updates.len(), 1);
    b = apply(&a, &b, Mode::Personal);
    assert_converged(&a, &b, Mode::Personal);
    assert!(!b.items[0].archived);

    b.items[0].archived = true;
    let changed = plan(&a, &b, Mode::Personal);
    assert!(changed.updates[0].changed_in_dest.archive);
    assert!(matches!(
        check(&changed, &b, &Safety::default()),
        Err(SafetyViolation::Overwrites(_))
    ));
}

#[test]
fn repeated_reversals_protect_edits_back_to_each_older_baseline() {
    let mut source = vault(vec![login("a1", "One", "pw1")]);
    let mut dest = mirrored(&source, Mode::Personal);
    let mut older = vec![source.items[0].content["notes"].clone()];
    for version in 1..=5 {
        std::mem::swap(&mut source, &mut dest);
        source.items[0]
            .content
            .insert("notes".into(), text(&format!("version {version}")));
        let before_id = dest.items[0].id.clone();
        let next = plan(&source, &dest, Mode::Personal);
        assert_eq!(
            check(
                &next,
                &dest,
                &Safety {
                    allow_reverse: true,
                    ..Safety::default()
                }
            ),
            Ok(())
        );
        dest = apply(&source, &dest, Mode::Personal);
        assert_eq!(dest.items[0].id, before_id);
        assert_converged(&source, &dest, Mode::Personal);
        for baseline in &older {
            let mut edited = dest.clone();
            edited.items[0]
                .content
                .insert("notes".into(), baseline.clone());
            let next = plan(&source, &edited, Mode::Personal);
            assert!(next.updates[0].changed_in_dest.content);
            assert!(matches!(
                check(&next, &edited, &Safety::default()),
                Err(SafetyViolation::Overwrites(_))
            ));
        }
        older.push(source.items[0].content["notes"].clone());
    }
}

#[test]
fn interrupted_archive_markers_remember_the_source_state_in_both_directions() {
    for wanted in [true, false] {
        let mut a = vault(vec![login("a1", "One", "pw1")]);
        a.items[0].archived = !wanted;
        let mut b = mirrored(&a, Mode::Personal);
        b.items[0].archived = wanted;
        let next = plan(&b, &a, Mode::Personal);
        let entry = &next.entries[0];
        let initial = entry
            .draft(&b, &HashMap::new(), &HashMap::new(), !wanted, Some(&[]))
            .unwrap();
        let final_draft = entry
            .draft(&b, &HashMap::new(), &HashMap::new(), wanted, Some(&[]))
            .unwrap();
        let final_item = Item {
            content: final_draft.content,
            ..a.items[0].clone()
        };
        for after_endpoint in [false, true] {
            let mut interrupted = a.clone();
            interrupted.items[0].content = initial.content.clone();
            interrupted.items[0].archived = if after_endpoint { wanted } else { !wanted };
            assert_eq!(
                marker(&interrupted.items[0]).unwrap().history,
                marker(&final_item).unwrap().history
            );
            let retry = plan(&b, &interrupted, Mode::Personal);
            assert!(!retry.updates[0].changed_in_dest.any());
            let repaired = apply(&b, &interrupted, Mode::Personal);
            assert_converged(&b, &repaired, Mode::Personal);

            let back = plan(&interrupted, &b, Mode::Personal);
            assert!(
                back.updates
                    .iter()
                    .all(|update| !update.changed_in_dest.any())
            );
            if after_endpoint {
                let mut edited = b.clone();
                edited.items[0].archived = !wanted;
                let refused = plan(&interrupted, &edited, Mode::Personal);
                assert!(refused.updates[0].changed_in_dest.archive);
                assert!(matches!(
                    check(&refused, &edited, &Safety::default()),
                    Err(SafetyViolation::Overwrites(_))
                ));
            }
        }
    }
}

#[test]
fn incomplete_archive_copies_are_not_deleted_as_complete_duplicates() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].archived = true;
    let next = plan(&source, &Vault::default(), Mode::Personal);
    let draft = next.entries[0]
        .draft(&source, &HashMap::new(), &HashMap::new(), false, Some(&[]))
        .unwrap();
    let first = Item {
        content: draft.content,
        ..login("copy1", "", "")
    };
    let second = Item {
        id: "copy2".into(),
        ..first.clone()
    };
    let dest = vault(vec![first, second]);
    let next = plan(&source, &dest, Mode::Personal);
    assert!(next.deletes.is_empty());
    assert_eq!(next.detached, 1);
    let after = apply(&source, &dest, Mode::Personal);
    assert_eq!(after.items.len(), 2);
    assert!(!after.items[1].archived);
    assert!(!plan(&source, &after, Mode::Personal).has_changes());
}

#[test]
fn newer_pending_attachments_cannot_be_certified_by_an_older_marker() {
    let mut a = vault(vec![login("a1", "One", "pw1")]);
    a.items[0].attachments = vec![attachment("f1", "scan.pdf", 100)];
    let mut b = mirrored(&a, Mode::Personal);
    a = apply(&b, &a, Mode::Personal);
    let next = plan(&a, &b, Mode::Personal);
    let draft = next.entries[0]
        .draft(&a, &HashMap::new(), &HashMap::new(), false, None)
        .unwrap();
    b.items[0].content = draft.content;
    for (source, dest) in [(&a, &b), (&b, &a)] {
        let next = plan(source, dest, Mode::Personal);
        assert!(next.updates[0].sync_attachments);
        assert!(next.updates[0].changed_in_dest.files);
        assert!(matches!(
            check(&next, dest, &Safety::default()),
            Err(SafetyViolation::Overwrites(_))
        ));
        let mut empty = dest.clone();
        empty.items[0].attachments.clear();
        let repair = plan(source, &empty, Mode::Personal);
        assert!(!repair.updates[0].changed_in_dest.files);
        assert_converged(
            source,
            &apply(source, &empty, Mode::Personal),
            Mode::Personal,
        );
    }
}

#[test]
fn legacy_copies_stay_unchanged_and_needed_writes_establish_order() {
    let mut a = vault(vec![login("a1", "One", "pw1")]);
    a.items[0].attachments = vec![attachment("f1", "file.txt", 10)];
    let mut b = mirrored(&a, Mode::Personal);
    let mut legacy = marker(&b.items[0]).unwrap();
    legacy.history = History::Unordered;
    set_marker(&mut b.items[0], legacy);
    assert_converged(&a, &b, Mode::Personal);
    b.items[0].content.insert("notes".into(), text("second"));
    a = apply(&b, &a, Mode::Personal);
    a.items[0].content.insert("notes".into(), text("third"));
    let next = plan(&a, &b, Mode::Personal);
    assert_eq!(check(&next, &b, &Safety::default()), Ok(()));
    b = apply(&a, &b, Mode::Personal);
    assert_converged(&a, &b, Mode::Personal);
    let reverse = plan(&b, &a, Mode::Personal);
    assert_eq!(check(&reverse, &a, &Safety::default()), Ok(()));
    a = apply(&b, &a, Mode::Personal);
    assert_converged(&b, &a, Mode::Personal);
}

#[test]
fn unordered_or_equal_generation_conflicts_do_not_authorize_overwrites() {
    let mut a = vault(vec![login("a1", "One", "pw1")]);
    a.items[0].attachments = vec![attachment("f1", "file.txt", 10)];
    let mut b = mirrored(&a, Mode::Personal);
    b.items[0].content.insert("notes".into(), text("second"));
    b.items[0].archived = true;
    b.items[0].attachments = vec![attachment("f2", "file.txt", 20)];
    a = apply(&b, &a, Mode::Personal);
    a.items[0].content.insert("notes".into(), text("third"));
    a.items[0].archived = false;
    for ordered in [false, true] {
        let mut source = a.clone();
        let mut dest = b.clone();
        for item in [&mut source.items[0], &mut dest.items[0]] {
            let mut own = marker(item).unwrap();
            own.history = if ordered {
                History::Ordered {
                    generation: NonZeroU64::new(7).unwrap(),
                    source_archived: own.history.source_archived().unwrap(),
                }
            } else {
                History::Unordered
            };
            set_marker(item, own);
        }
        let next = plan(&source, &dest, Mode::Personal);
        assert_eq!(
            next.updates[0].changed_in_dest,
            Changes {
                content: true,
                archive: true,
                files: true
            }
        );
        assert!(matches!(
            check(&next, &dest, &Safety::default()),
            Err(SafetyViolation::Overwrites(_))
        ));
    }
}

#[test]
fn only_an_exact_reverse_link_supplies_a_baseline_but_all_observed_order_is_kept() {
    let mut a = vault(vec![login("a1", "One", "pw1")]);
    let mut b = mirrored(&a, Mode::Personal);
    a = apply(&b, &a, Mode::Personal);
    let mut link = marker(&a.items[0]).unwrap();
    link.source_id = "another-copy".into();
    link.history = History::Ordered {
        generation: NonZeroU64::new(20).unwrap(),
        source_archived: false,
    };
    set_marker(&mut a.items[0], link);
    b.items[0]
        .content
        .insert("notes".into(), text("destination edit"));
    let next = plan(&a, &b, Mode::Personal);
    assert!(next.updates[0].changed_in_dest.content);
    let after = apply(&a, &b, Mode::Personal);
    assert_eq!(marker(&after.items[0]).unwrap().history.generation(), 21);
    assert_converged(&a, &after, Mode::Personal);
}

#[test]
fn marker_generation_exhaustion_fails_only_a_needed_write() {
    let mut a = vault(vec![login("a1", "One", "pw1")]);
    let mut b = mirrored(&a, Mode::Personal);
    let mut own = marker(&b.items[0]).unwrap();
    own.history = History::Ordered {
        generation: NonZeroU64::new(u64::MAX).unwrap(),
        source_archived: false,
    };
    set_marker(&mut b.items[0], own);
    assert_converged(&a, &b, Mode::Personal);
    a.items[0].content.insert("notes".into(), text("changed"));
    let next = plan(&a, &b, Mode::Personal);
    assert_eq!(next.updates.len(), 1);
    assert_eq!(
        next.entries[0].draft(&a, &HashMap::new(), &HashMap::new(), false, Some(&[])),
        Err(PlanError::GenerationExhausted)
    );
}

#[test]
fn a_clone_listed_first_cannot_make_a_reverted_linked_copy_deletable() {
    for component in ["content", "archive", "attachments"] {
        let mut a = vault(vec![login("a1", "One", "pw1")]);
        a.items[0].attachments = vec![attachment("f1", "file.txt", 10)];
        let initial = mirrored(&a, Mode::Personal);
        let mut b = initial.clone();
        match component {
            "content" => {
                b.items[0].content.insert("notes".into(), text("state2"));
            }
            "archive" => b.items[0].archived = true,
            "attachments" => b.items[0].attachments = vec![attachment("f2", "file.txt", 20)],
            _ => unreachable!(),
        }
        a = apply(&b, &a, Mode::Personal);
        a.items[0].content.insert("notes".into(), text("state1"));
        if component == "attachments" {
            a.items[0].attachments = vec![attachment("f3", "file.txt", 30)];
        }
        b = initial;
        let reverted = plan(&a, &b, Mode::Personal);
        assert!(reverted.updates[0].changed_in_dest.any(), "{component}");
        let clone = Item {
            id: "clone".into(),
            ..b.items[0].clone()
        };
        b.items.insert(0, clone);
        let next = plan(&a, &b, Mode::Personal);
        assert!(next.deletes.is_empty(), "{component}");
        assert_eq!(next.detached, 1);
        assert_eq!(next.updates.len(), 1);
        assert_eq!(b.items[next.updates[0].dest].id, "clone");
        assert_eq!(check(&next, &b, &Safety::default()), Ok(()));
        let after = apply(&a, &b, Mode::Personal);
        assert_eq!(after.items.len(), 2);
        assert_eq!(after.items[1], b.items[1]);
        assert!(!plan(&a, &after, Mode::Personal).has_changes());
    }
}

#[test]
fn mixed_version_copies_converge_in_either_server_order() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    let older = mirrored(&source, Mode::Personal).items.remove(0);
    source.items[0]
        .content
        .insert("notes".into(), text("newer"));
    let current = mirrored(&source, Mode::Personal).items.remove(0);

    for old_first in [true, false] {
        let old = Item {
            id: "older".into(),
            ..older.clone()
        };
        let new = Item {
            id: "current".into(),
            ..current.clone()
        };
        let dest = vault(if old_first {
            vec![old, new]
        } else {
            vec![new, old]
        });
        let planned = plan(&source, &dest, Mode::Personal);
        assert_eq!(check(&planned, &dest, &Safety::default()), Ok(()));
        assert!(planned.deletes.is_empty());
        assert_eq!(planned.detached, 1);
        assert!(planned.updates.is_empty());
        let after = apply(&source, &dest, Mode::Personal);
        assert!(!plan(&source, &after, Mode::Personal).has_changes());
        assert_eq!(after.items.len(), 2);
        assert_eq!(
            after
                .items
                .iter()
                .find(|i| i.id == "older")
                .unwrap()
                .content,
            older.content
        );
        assert_eq!(
            after
                .items
                .iter()
                .find(|i| i.id == "current")
                .unwrap()
                .content,
            current.content
        );
    }
}

#[test]
fn switching_over_a_replaced_attachment_counts_as_a_change_in_the_destination() {
    let mut a = vault(vec![login("a1", "One", "pw1")]);
    a.items[0].attachments = vec![attachment("f1", "scan.pdf", 100)];
    let b = mirrored(&a, Mode::Personal);
    a.items[0].attachments = vec![attachment("f2", "scan.pdf", 150)];

    let update = &plan(&b, &a, Mode::Personal).updates[0];
    assert!(update.sync_attachments && update.changed_in_dest.any());
}

#[test]
fn an_original_is_never_deleted_as_a_duplicate() {
    let original = vault(vec![login("a1", "One", "pw1")]);
    let copies = mirrored(&original, Mode::Personal);
    // The original was in the trash when the copies were mirrored back, so
    // that made a second item, listed first.
    let mut trashed = original.clone();
    trashed.items[0].in_trash = true;
    let mut a = apply(&copies, &trashed, Mode::Personal);
    a.items[0].in_trash = false;
    a.items.reverse();

    let plan = plan(&copies, &a, Mode::Personal);
    assert!(plan.deletes.is_empty());
    assert_eq!((plan.untracked, plan.reversed), (1, 0));
}

#[test]
fn a_marker_naming_a_deleted_item_gives_way_to_the_source_items_marker() {
    let a = vault(vec![login("a1", "One", "pw1"), login("a2", "Two", "pw2")]);
    let mut b = mirrored(&a, Mode::Personal);
    // So that no later copy gets its id.
    b.items[0].id = "deleted".into();
    let a = apply(&b, &a, Mode::Personal);
    b.items.remove(0);
    let b = apply(&a, &b, Mode::Personal);

    let plan = plan(&b, &a, Mode::Personal);
    assert_eq!((plan.creates.len(), plan.deletes.len()), (0, 0));
    assert_eq!(plan.reversed, 0);
    let a = apply(&b, &a, Mode::Personal);
    assert_converged(&b, &a, Mode::Personal);
    assert_converged(&a, &b, Mode::Personal);
}

#[test]
fn a_clone_of_a_pending_copy_with_its_own_attachment_is_left_alone() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let mut dest = mirrored(&source, Mode::Personal);
    dest.items[0] = pending(&source, &dest.items[0]);
    let mut clone = dest.items[0].clone();
    clone.id = "clone".into();
    clone.attachments = vec![attachment("mine", "mine.txt", 5)];
    let copy = dest.items[0].id.clone();

    for clone_first in [false, true] {
        let mut dest = dest.clone();
        dest.items.insert(usize::from(!clone_first), clone.clone());
        let plan = plan(&source, &dest, Mode::Personal);
        assert!(plan.deletes.is_empty());
        assert_eq!(plan.detached, 1);
        assert_eq!(dest.items[plan.updates[0].dest].id, copy);
    }
}

#[test]
fn only_complete_copies_of_one_source_version_count_as_duplicates() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    let second = |dest: &Vault, item: Item| {
        let mut dest = dest.clone();
        dest.items.push(Item {
            id: "second".into(),
            ..item
        });
        dest
    };
    let deletes = |source: &Vault, dest: &Vault| plan(source, dest, Mode::Personal).deletes.len();
    let dest = mirrored(&source, Mode::Personal);
    assert_eq!(deletes(&source, &second(&dest, dest.items[0].clone())), 1);

    // Left by a run that stopped before its attachments were copied.
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let dest = mirrored(&source, Mode::Personal);
    let unfinished = second(&dest, pending(&source, &dest.items[0]));
    assert_eq!(deletes(&source, &unfinished), 0);
    assert_eq!(plan(&source, &unfinished, Mode::Personal).detached, 1);

    // Copied from an older version of the source item.
    let old = dest.items[0].clone();
    source.items[0]
        .content
        .insert("notes".into(), text("newer"));
    let dest = apply(&source, &dest, Mode::Personal);
    assert_eq!(deletes(&source, &second(&dest, old)), 0);
}

#[test]
fn markers_of_another_pair_of_vaults_grant_nothing() {
    let source = numbered(2);
    let mirror = mirrored(&source, Mode::Personal);
    // The same copies, as another mirror into this destination made them.
    let foreign = super::plan(&source, &mirror, Mode::Personal, OTHER_PAIR);
    assert_eq!(foreign.creates.len(), 2);
    assert_eq!((foreign.untracked, foreign.tracked_before), (2, 0));
    assert!(foreign.updates.is_empty() && foreign.deletes.is_empty());
    let gone = super::plan(&vault(vec![]), &mirror, Mode::Personal, OTHER_PAIR);
    assert!(gone.deletes.is_empty());
    assert!(marker_of(&mirror.items[0], OTHER_PAIR).is_none());
    // Copying a copy on, into a third vault, carries no marker of the first pair.
    let onward = apply_pair(&mirror, OTHER_PAIR);
    for item in &onward.items {
        let markers: Vec<_> = (item.fields().iter())
            .filter_map(field_marker)
            .map(|marker| marker.pair)
            .collect();
        assert_eq!(markers, [OTHER_PAIR]);
    }
}

fn apply_pair(source: &Vault, pair: &str) -> Vault {
    let plan = super::plan(source, &Vault::default(), Mode::Personal, pair);
    let items = (plan.entries.iter())
        .map(|entry| {
            let draft =
                (entry.draft(source, &HashMap::new(), &HashMap::new(), false, Some(&[]))).unwrap();
            Item {
                content: draft.content,
                ..login(&format!("third-{}", entry.source), "", "")
            }
        })
        .collect();
    vault(items)
}

#[test]
fn a_source_field_named_like_the_marker_is_copied() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].content.insert(
        "fields".into(),
        Node::List(vec![map(vec![
            ("name", text(MARKER_FIELD)),
            ("value", text("my own note")),
        ])]),
    );
    let dest = mirrored(&source, Mode::Personal);

    let fields = dest.items[0].fields();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].as_map().unwrap()["value"], text("my own note"));
    assert!(is_mirror_marker(&fields[1]));
}

#[test]
fn an_edited_clone_of_a_copy_is_left_alone() {
    let source = numbered(1);
    let mut dest = mirrored(&source, Mode::Personal);
    let mut clone = dest.items[0].clone();
    clone.id = "clone".into();
    clone.content.insert("notes".into(), text("my variant"));
    // Listed first, so only its edit tells it apart from the copy.
    dest.items.insert(0, clone);

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.detached, 1);
    assert!(plan.deletes.is_empty() && plan.updates.is_empty());
    assert_eq!(plan.unchanged, 1);
}

#[test]
fn an_attachment_added_to_a_copy_counts_as_a_change_in_the_destination() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let mut dest = mirrored(&source, Mode::Personal);
    dest.items[0]
        .attachments
        .push(attachment("extra", "mine.txt", 5));

    let update = &plan(&source, &dest, Mode::Personal).updates[0];
    assert!(update.sync_attachments && update.changed_in_dest.any());
    // A source change alone is not one.
    let mut dest = mirrored(&source, Mode::Personal);
    source.items[0].attachments = vec![attachment("a2", "scan.pdf", 120)];
    dest.items[0].revision_date = None;
    let update = &plan(&source, &dest, Mode::Personal).updates[0];
    assert!(update.sync_attachments && !update.changed_in_dest.any());
}

#[test]
fn what_vault_mirror_did_not_make_is_left_alone() {
    let source = numbered(3);
    let mut dest = vault(vec![login("existing", "Already here", "pw")]);
    dest.folders = vec![folder("own", "Mine"), folder("empty", "Leftover")];
    dest.items[0].folder_id = Some("own".into());

    // A first run into a destination that is in use.
    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!((plan.creates.len(), plan.deletes.len()), (3, 0));
    assert_eq!(plan.untracked, 1);
    assert_eq!(check(&plan, &dest, &Safety::default()), Ok(()));
    let after = apply(&source, &dest, Mode::Personal);
    assert_converged(&source, &after, Mode::Personal);
    assert_eq!(
        after.folders.len(),
        2,
        "an empty folder of the user's stays"
    );
    let kept = after
        .items
        .iter()
        .find(|item| item.id == "existing")
        .unwrap();
    assert_eq!(kept, &dest.items[0]);

    // Later runs leave it alone too.
    let again = super::plan(&source, &after, Mode::Personal, PAIR);
    assert!(!again.has_changes());
    assert_eq!(again.untracked, 1);
}

#[test]
fn a_collection_holding_an_item_vault_mirror_did_not_make_is_kept() {
    let mut source = numbered(1);
    source.collections = vec![collection("c1", "Shared")];
    source.items[0].collection_ids = vec!["c1".into()];
    let mut dest = mirrored(&source, Mode::Organization);
    dest.collections.push(collection("own", "Team"));
    let mut own = login("own-item", "Team login", "pw");
    own.collection_ids = vec!["own".into()];
    dest.items.push(own);

    assert!(!plan(&source, &dest, Mode::Organization).has_changes());
}

#[test]
fn deleting_more_than_half_of_the_copies_is_refused() {
    let confirmed = Safety {
        allow_mass_delete: true,
        ..Safety::default()
    };
    let refused = |copies: usize, kept: usize| {
        let mut dest = mirrored(&numbered(copies), Mode::Personal);
        // Neither the user's items nor another pair's copies count.
        dest.items.push(login("mine", "Mine", "pw"));
        let others = super::plan(&numbered(3), &Vault::default(), Mode::Personal, OTHER_PAIR);
        assert_eq!(others.creates.len(), 3);
        dest.items
            .extend(apply_pair(&numbered(3), OTHER_PAIR).items);
        let plan = plan(&numbered(kept), &dest, Mode::Personal);
        assert_eq!(check(&plan, &dest, &confirmed), Ok(()));
        match check(&plan, &dest, &Safety::default()) {
            Ok(()) => None,
            Err(SafetyViolation::MassDelete { deleted, total }) => Some((deleted, total)),
            Err(other) => panic!("{other}"),
        }
    };
    assert_eq!(refused(0, 0), None);
    assert_eq!(refused(1, 0), Some((1, 1)));
    assert_eq!(refused(2, 1), None, "exactly half");
    assert_eq!(refused(3, 1), Some((2, 3)));
    assert_eq!(refused(40, 0), Some((40, 40)));

    // The source is another vault: everything would be replaced.
    let dest = mirrored(&numbered(40), Mode::Personal);
    let other = vault(
        (0..40)
            .map(|i| login(&format!("other{i}"), &format!("Item {i}"), "pw"))
            .collect(),
    );
    assert_eq!(
        check(
            &plan(&other, &dest, Mode::Personal),
            &dest,
            &Safety::default()
        ),
        Err(SafetyViolation::MassDelete {
            deleted: 40,
            total: 40
        })
    );
}

#[test]
fn markers_accept_exactly_the_legacy_or_ordered_format() {
    let legacy = format!("{PAIR} s1 {} 0 - -", "ab".repeat(32));
    let ordered = format!("{legacy} 42 1");
    for value in [&legacy, &ordered] {
        assert_eq!(Marker::parse(value).unwrap().render(), *value);
    }
    for suffix in [
        " 1",
        " 0 1",
        " -1 1",
        " +1 1",
        " 18446744073709551616 1",
        " x 1",
        " 1 2",
        " 1 1 extra",
    ] {
        assert!(Marker::parse(&format!("{legacy}{suffix}")).is_none());
    }
}

#[test]
fn a_marker_from_before_pairs_is_no_marker() {
    let source = numbered(1);
    let mut dest = mirrored(&source, Mode::Personal);
    let Some(Node::List(fields)) = dest.items[0].content.get_mut("fields") else {
        panic!()
    };
    let legacy = format!("s0 {} - -", "ab".repeat(32));
    fields[0] = map(vec![("name", text(MARKER_FIELD)), ("value", text(&legacy))]);
    assert!(!is_mirror_marker(&fields[0]));
    assert_eq!(marker(&dest.items[0]), None);
    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!((plan.creates.len(), plan.untracked), (1, 1));
}

#[test]
fn differences_are_reported_as_paths_without_values() {
    let left = json!({"content": {"name": "a", "login": {"password": "secret-1", "uris": [{"uri": "x"}]}}, "folder": null});
    let right = json!({"content": {"name": "a", "login": {"password": "secret-2", "uris": [{"uri": "y"}], "totp": "t"}}, "folder": "F"});
    let paths = differing_paths(&left, &right);
    assert_eq!(
        paths,
        [
            "content.login.password",
            "content.login.totp",
            "content.login.uris[0].uri",
            "folder"
        ]
    );
    assert!(differing_paths(&left, &left).is_empty());
    assert_eq!(differing_paths(&json!([1]), &json!([1, 2])), ["<root>"]);
}

const STAMP: &str = "3333333333333333333333333333333333333333333333333333333333333333";

fn made(ids: &[&str]) -> BTreeSet<Receipt> {
    (ids.iter())
        .map(|id| Receipt {
            folder_id: id.to_string(),
            stamp: STAMP.into(),
        })
        .collect()
}

fn stamped(id: &str, name: &str) -> Folder {
    Folder {
        stamp: Some(STAMP.into()),
        ..folder(id, name)
    }
}

fn filed(id: &str, folder: &str) -> Item {
    Item {
        folder_id: Some(folder.into()),
        ..login(id, id, "pw")
    }
}

fn ids(receipts: &[Receipt]) -> Vec<&str> {
    receipts.iter().map(|r| r.folder_id.as_str()).collect()
}

#[test]
fn folder_receipts_name_the_destination_as_well_as_the_pair() {
    assert_ne!(owner(PAIR, "a"), owner(PAIR, "b"));
    assert_ne!(owner(PAIR, "a"), owner(OTHER_PAIR, "a"));
}

#[test]
fn only_empty_unselected_folders_a_run_made_can_go() {
    let source = Vault {
        folders: vec![folder("s1", "Work"), folder("s2", "Empty")],
        ..Vault::default()
    };
    let mut trashed = filed("trashed", "in-trash");
    trashed.in_trash = true;
    let mut archived = filed("archived", "in-archive");
    archived.archived = true;
    let dest = Vault {
        folders: vec![
            stamped("old-b", "Old"),
            stamped("old-a", "Old"),
            stamped("theirs", "Old"),
            stamped("selected", "Work"),
            stamped("empty", "Empty"),
            stamped("used", "Used"),
            stamped("in-trash", "Trash"),
            stamped("in-archive", "Archive"),
            stamped("elsewhere", "Organization"),
            Folder {
                stamp: Some("4".repeat(64)),
                ..folder("renamed", "Renamed")
            },
            folder("unknown", "From a snapshot"),
            stamped("twice", "Twice"),
            stamped("twice", "Twice"),
        ],
        items: vec![filed("own", "used"), trashed, archived],
        folders_used_elsewhere: BTreeSet::from(["elsewhere".to_string()]),
        ..Vault::default()
    };
    let receipts = made(&[
        "old-b",
        "old-a",
        "selected",
        "empty",
        "used",
        "in-trash",
        "in-archive",
        "elsewhere",
        "renamed",
        "unknown",
        "twice",
        "gone",
    ]);
    let cleanup = folder_cleanup(&receipts, &source, &dest, None);
    // A same-name folder vault-mirror didn't make stays, and the order is
    // the receipts', whatever the server's.
    assert_eq!(ids(&cleanup.obsolete), ["old-a", "old-b"]);
    assert_eq!(ids(&cleanup.stale), ["gone", "renamed"]);
}

#[test]
fn copies_the_plan_moves_leave_a_folder_and_copies_it_trashes_do_not() {
    let original = Vault {
        folders: vec![folder("s1", "Old")],
        items: vec![filed("a", "s1"), filed("b", "s1")],
        ..Vault::default()
    };
    let mut dest = apply(&original, &Vault::default(), Mode::Personal);
    for folder in &mut dest.folders {
        folder.stamp = Some(STAMP.into());
    }
    let receipts = made(&[dest.folders[0].id.as_str()]);
    let renamed = |items: Vec<Item>| Vault {
        folders: vec![folder("s1", "New")],
        items,
        ..Vault::default()
    };

    let moved = renamed(original.items.clone());
    let planned = plan(&moved, &dest, Mode::Personal);
    assert_eq!(planned.updates.len(), 2);
    assert_eq!(planned.folders_to_create, ["New"]);
    let cleanup = folder_cleanup(&receipts, &moved, &dest, Some(&planned));
    assert_eq!(cleanup.obsolete.len(), 1);
    // Until the copies are moved, a read shows them in the folder.
    assert!(
        folder_cleanup(&receipts, &moved, &dest, None)
            .obsolete
            .is_empty()
    );

    let one_deleted = renamed(vec![original.items[0].clone()]);
    let planned = plan(&one_deleted, &dest, Mode::Personal);
    assert_eq!(planned.deletes.len(), 1);
    let cleanup = folder_cleanup(&receipts, &one_deleted, &dest, Some(&planned));
    assert!(
        cleanup.obsolete.is_empty(),
        "the trashed copy is still in it"
    );
}
