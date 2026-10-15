use std::collections::{BTreeMap, HashMap};

use serde_json::json;

use super::*;
use crate::vault::Attachment;

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

fn login(id: &str, name: &str, password: &str) -> Item {
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
                ("password", text(password)),
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

/// Applies a plan the way the engine does, without a server: the destination
/// ends up holding exactly what the drafts describe.
fn apply(source: &Vault, dest: &Vault, mode: Mode) -> Vault {
    apply_with(source, dest, mode, true)
}

fn apply_with(source: &Vault, dest: &Vault, mode: Mode, copy_attachments: bool) -> Vault {
    let plan = plan_with(source, dest, mode, copy_attachments);
    let mut next = dest.clone();
    let mut counter = dest.items.len() + dest.folders.len() + dest.collections.len();
    let mut fresh = |prefix: &str| {
        counter += 1;
        format!("{prefix}-{counter}")
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

    let write = |item: &mut Item, entry: &Entry| {
        let draft = entry.draft(source, &folder_ids, &collection_ids).unwrap();
        item.content = draft.content;
        item.folder_id = draft.folder_id;
        item.collection_ids = draft.collection_ids;
    };
    // As the engine does once an item's attachments are copied.
    let finish = |item: &mut Item, entry: &Entry| {
        if item.attachments.is_empty() {
            return;
        }
        let draft = entry
            .finished_draft(source, item, &folder_ids, &collection_ids)
            .unwrap();
        item.content = draft.content;
    };
    for &entry_index in &plan.creates {
        let entry = &plan.entries[entry_index];
        let source_item = &source.items[entry.source];
        let mut item = login(&fresh("dest-item"), "", "");
        write(&mut item, entry);
        item.archived = source_item.archived;
        if copy_attachments {
            item.attachments = source_item
                .attachments
                .iter()
                .map(|a| attachment(&fresh("dest-attachment"), &a.file_name, a.size))
                .collect();
            finish(&mut item, entry);
        }
        next.items.push(item);
    }
    for update in &plan.updates {
        let entry = &plan.entries[update.entry];
        let source_item = &source.items[entry.source];
        let attachments: Vec<Attachment> = source_item
            .attachments
            .iter()
            .map(|a| attachment(&fresh("dest-attachment"), &a.file_name, a.size))
            .collect();
        let item = &mut next.items[update.dest];
        if update.write_content {
            write(item, entry);
        }
        if update.sync_attachments {
            item.attachments = attachments;
            finish(item, entry);
        }
        if let Some(archived) = update.set_archived {
            item.archived = archived;
        }
    }
    let doomed: Vec<String> = plan
        .deletes
        .iter()
        .map(|d| dest.items[d.dest].id.clone())
        .collect();
    next.items.retain(|item| !doomed.contains(&item.id));
    next.folders.retain(|f| !plan.folders_to_delete.contains(f));
    next.collections
        .retain(|c| !plan.collections_to_delete.contains(c));
    next
}

fn assert_converged(source: &Vault, dest: &Vault, mode: Mode) {
    let again = plan(source, dest, mode);
    assert!(!again.has_changes(), "plan still has changes: {again:#?}");
    let live = source.items.iter().filter(|item| !item.in_trash).count();
    assert_eq!(again.unchanged, live);
    let copies = dest
        .items
        .iter()
        .filter(|item| !item.in_trash && marker_of(item).is_some())
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
fn the_marker_is_the_last_field_and_names_the_source_item() {
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
    assert!(is_marker_field(&fields[1]));
    assert_eq!(fields[1].as_map().unwrap()["type"], Node::Number(1.into()));
    let marker = marker_of(&dest.items[0]).unwrap();
    assert_eq!(marker.source_id, "s1");
    assert_eq!(
        (marker.source_files.as_str(), marker.dest_files.as_str()),
        ("-", "-")
    );
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
            changed_in_dest: false,
            sync_attachments: false,
            set_archived: None
        }]
    );
    assert!(plan.creates.is_empty() && plan.deletes.is_empty());

    let dest = apply(&source, &dest, Mode::Personal);
    assert_converged(&source, &dest, Mode::Personal);
    // Updated in place: the destination item keeps its identity.
    assert_eq!(dest.items[1].id, "dest-item-2");
}

#[test]
fn a_hand_edited_destination_item_is_repaired_although_its_marker_is_current() {
    let source = vault(vec![login("s1", "One", "pw1")]);
    let mut dest = mirrored(&source, Mode::Personal);
    let Some(Node::Map(login)) = dest.items[0].content.get_mut("login") else {
        panic!()
    };
    login.insert("password".into(), text("tampered"));

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.updates.len(), 1);
    assert!(plan.updates[0].write_content);
    assert!(plan.updates[0].changed_in_dest);
    assert_converged(
        &source,
        &apply(&source, &dest, Mode::Personal),
        Mode::Personal,
    );
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
    assert_eq!(marker_of(&dest.items[0]), None);
    let without_attachments = format!("s1 {}", "0123abcd".repeat(8));
    let Some(Node::List(fields)) = dest.items[0].content.get_mut("fields") else {
        panic!()
    };
    fields[0] = map(vec![
        ("name", text(MARKER_FIELD)),
        ("value", text(&without_attachments)),
    ]);

    assert_eq!(marker_of(&dest.items[0]), None);
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

    assert_eq!(marker_of(&dest.items[0]), None);
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
            reason: DeleteReason::Stale
        }]
    );
    assert_converged(
        &source,
        &apply(&source, &dest, Mode::Personal),
        Mode::Personal,
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

    // An attachment removed from the destination by hand is restored.
    let mut damaged = dest.clone();
    damaged.items[0].attachments.clear();
    let update = &plan(&source, &damaged, Mode::Personal).updates[0];
    assert!(update.sync_attachments);

    // Removing the last source attachment removes it from the destination.
    let mut removed = source.clone();
    removed.items[0].attachments.clear();
    let after = apply(&removed, &dest, Mode::Personal);
    assert!(after.items[0].attachments.is_empty());
    assert_converged(&removed, &after, Mode::Personal);
}

#[test]
fn attachments_the_destination_cannot_take_follow_once_it_can() {
    let mut source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];

    let first = plan_with(&source, &Vault::default(), Mode::Personal, false);
    assert_eq!(first.attachments_withheld, 1);
    assert_eq!(first.summary(&source).attachments_copied, 0);
    let dest = apply_with(&source, &Vault::default(), Mode::Personal, false);
    assert!(dest.items.iter().all(|item| item.attachments.is_empty()));
    let again = plan_with(&source, &dest, Mode::Personal, false);
    assert!(!again.has_changes(), "{again:#?}");
    assert_eq!(again.attachments_withheld, 1);

    // A content edit is still mirrored, and the marker goes on saying the
    // attachment is missing.
    let mut edited = source.clone();
    edited.items[0]
        .content
        .insert("name".into(), text("Renamed"));
    let dest = apply_with(&edited, &dest, Mode::Personal, false);
    assert!(!plan_with(&edited, &dest, Mode::Personal, false).has_changes());

    let able = plan(&edited, &dest, Mode::Personal);
    assert_eq!(able.updates.len(), 1);
    assert!(able.updates[0].sync_attachments);
    assert_converged(
        &edited,
        &apply(&edited, &dest, Mode::Personal),
        Mode::Personal,
    );
}

#[test]
fn attachments_already_mirrored_are_kept_when_no_more_can_be_copied() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let dest = mirrored(&source, Mode::Personal);
    assert!(!plan_with(&source, &dest, Mode::Personal, false).has_changes());

    source.items[0]
        .attachments
        .push(attachment("a2", "new.pdf", 50));
    let withheld = plan_with(&source, &dest, Mode::Personal, false);
    assert!(!withheld.has_changes(), "{withheld:#?}");
    assert_eq!(withheld.attachments_withheld, 1);
    assert!(plan(&source, &dest, Mode::Personal).updates[0].sync_attachments);
}

#[test]
fn an_item_left_with_a_pending_marker_gets_its_attachments_again() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let mut dest = mirrored(&source, Mode::Personal);

    // As a run leaves an item that stopped while copying: the files may be
    // incomplete although their names match.
    let plan_now = plan(&source, &Vault::default(), Mode::Personal);
    let draft = plan_now.entries[0]
        .pending_draft(&source, &HashMap::new(), &HashMap::new())
        .unwrap();
    dest.items[0].content = draft.content;

    let update = &plan(&source, &dest, Mode::Personal).updates[0];
    assert!(update.sync_attachments);
}

#[test]
fn a_destination_file_replaced_under_the_same_name_is_copied_again() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let mut dest = mirrored(&source, Mode::Personal);
    dest.items[0].attachments = vec![attachment("replaced", "scan.pdf", 100)];
    let plan = plan(&source, &dest, Mode::Personal);
    assert!(plan.updates[0].sync_attachments);
    assert_converged(
        &source,
        &apply(&source, &dest, Mode::Personal),
        Mode::Personal,
    );
}

#[test]
fn a_source_file_replaced_under_the_same_name_counts_as_withheld() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let dest = mirrored(&source, Mode::Personal);
    source.items[0].attachments = vec![attachment("a2", "scan.pdf", 120)];
    assert_eq!(
        plan_with(&source, &dest, Mode::Personal, false).attachments_withheld,
        1
    );
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
fn archive_state_is_mirrored_without_rewriting_content() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    let dest = mirrored(&source, Mode::Personal);
    source.items[0].archived = true;

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(
        plan.updates,
        [Update {
            entry: 0,
            dest: 0,
            write_content: false,
            changed_in_dest: false,
            sync_attachments: false,
            set_archived: Some(true)
        }]
    );
    assert_converged(
        &source,
        &apply(&source, &dest, Mode::Personal),
        Mode::Personal,
    );
}

#[test]
fn a_renamed_folder_moves_its_items() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.folders = vec![folder("f1", "Old")];
    source.items[0].folder_id = Some("f1".into());
    let dest = mirrored(&source, Mode::Personal);
    source.folders[0].name = "New".into();

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.folders_to_create, ["New"]);
    assert_eq!(plan.folders_to_delete.len(), 1);
    assert_eq!(plan.updates.len(), 1);
    let after = apply(&source, &dest, Mode::Personal);
    assert_converged(&source, &after, Mode::Personal);
    assert_eq!(after.folders.len(), 1);
    assert_eq!(
        after.items[0].folder_id.as_deref(),
        Some(after.folders[0].id.as_str())
    );
}

#[test]
fn folders_sharing_a_name_are_merged() {
    let mut source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    source.folders = vec![folder("f1", "Same"), folder("f2", "Same")];
    source.items[0].folder_id = Some("f1".into());
    source.items[1].folder_id = Some("f2".into());
    let mut dest = mirrored(&source, Mode::Personal);
    assert_eq!(dest.folders.len(), 1);

    // A duplicate in the destination is removed and its items are moved.
    dest.folders.push(folder("dup", "Same"));
    dest.items[1].folder_id = Some("dup".into());
    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.folders_to_delete, [folder("dup", "Same")]);
    assert_eq!(plan.updates.len(), 1);
    assert_converged(
        &source,
        &apply(&source, &dest, Mode::Personal),
        Mode::Personal,
    );
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
    assert_eq!(plan.collections_to_delete.len(), 1);
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
        .draft(&source, &HashMap::new(), &HashMap::new())
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
        check_safety(&plan, &Safety::default()),
        Err(SafetyViolation::Reversed(3))
    );
    let reverse = Safety {
        allow_reverse: true,
        ..Safety::default()
    };
    assert_eq!(check_safety(&plan, &reverse), Ok(()));
    let changed: Vec<bool> = plan.updates.iter().map(|u| u.changed_in_dest).collect();
    assert_eq!(changed, [true, false, false]);

    let original = apply(&mirror, &original, Mode::Personal);
    assert_converged(&mirror, &original, Mode::Personal);
    let again = super::plan(&mirror, &original, Mode::Personal);
    assert_eq!(check_safety(&again, &Safety::default()), Ok(()));

    // And back: each item still has one marker, and nothing is duplicated.
    let back = super::plan(&original, &mirror, Mode::Personal);
    assert_eq!(back.reversed, 0);
    assert_eq!(check_safety(&back, &Safety::default()), Ok(()));
    assert!(!back.has_changes());
    for item in original.items.iter().chain(&mirror.items) {
        let markers = item.fields().iter().filter(|f| is_marker_field(f)).count();
        assert_eq!(markers, 1, "{}", item.id);
    }
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
    assert!(update.sync_attachments && update.changed_in_dest);
    // A source change alone is not one.
    let mut dest = mirrored(&source, Mode::Personal);
    source.items[0].attachments = vec![attachment("a2", "scan.pdf", 120)];
    dest.items[0].revision_date = None;
    let update = &plan(&source, &dest, Mode::Personal).updates[0];
    assert!(update.sync_attachments && !update.changed_in_dest);
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
    assert_eq!(plan.folders_to_delete, [folder("empty", "Leftover")]);
    assert_eq!(check_safety(&plan, &Safety::default()), Ok(()));
    let after = apply(&source, &dest, Mode::Personal);
    assert_converged(&source, &after, Mode::Personal);
    let kept = after
        .items
        .iter()
        .find(|item| item.id == "existing")
        .unwrap();
    assert_eq!(kept, &dest.items[0]);

    // Later runs leave it alone too.
    let again = super::plan(&source, &after, Mode::Personal);
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

    let plan = plan(&source, &dest, Mode::Organization);
    assert!(plan.collections_to_delete.is_empty());
    assert!(!plan.has_changes());
}

#[test]
fn a_folder_holding_anything_but_copies_is_kept() {
    let source = numbered(1);
    let mut dest = mirrored(&source, Mode::Personal);
    dest.folders = vec![
        folder("trash", "Restorable"),
        folder("org", "Shared"),
        folder("empty", "Leftover"),
    ];
    let mut trashed = login("own-item", "Old login", "pw");
    trashed.folder_id = Some("trash".into());
    trashed.in_trash = true;
    dest.items.push(trashed);
    dest.folders_used_elsewhere = ["org".to_string()].into();

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.folders_to_delete, [folder("empty", "Leftover")]);
}

#[test]
fn deleting_every_earlier_copy_is_refused() {
    let dest = mirrored(&numbered(40), Mode::Personal);
    let confirmed = Safety {
        allow_mass_delete: true,
        ..Safety::default()
    };

    // The source reads as empty.
    let emptied = plan(&numbered(0), &dest, Mode::Personal);
    assert_eq!(
        check_safety(&emptied, &Safety::default()),
        Err(SafetyViolation::ReplacesEverything(40))
    );
    assert_eq!(check_safety(&emptied, &confirmed), Ok(()));

    // The source is another vault: everything would be replaced.
    let other = vault(
        (0..40)
            .map(|i| login(&format!("other{i}"), &format!("Item {i}"), "pw"))
            .collect(),
    );
    assert_eq!(
        check_safety(&plan(&other, &dest, Mode::Personal), &Safety::default()),
        Err(SafetyViolation::ReplacesEverything(40))
    );

    // Deleting most items, but not all, is an ordinary change.
    assert_eq!(
        check_safety(
            &plan(&numbered(1), &dest, Mode::Personal),
            &Safety::default()
        ),
        Ok(())
    );
}

#[test]
fn a_delete_limit_applies_only_when_set() {
    let dest = mirrored(&numbered(40), Mode::Personal);
    let limited = Safety {
        max_delete_percent: Some(50),
        ..Safety::default()
    };

    let shrunk = plan(&numbered(5), &dest, Mode::Personal);
    assert_eq!(check_safety(&shrunk, &Safety::default()), Ok(()));
    assert_eq!(
        check_safety(&shrunk, &limited),
        Err(SafetyViolation::MassDelete {
            deleting: 35,
            total: 40,
            percent: 87,
            limit: 50
        })
    );
    let confirmed = Safety {
        allow_mass_delete: true,
        ..limited.clone()
    };
    assert_eq!(check_safety(&shrunk, &confirmed), Ok(()));

    // Just over half is refused, although it rounds down to the limit.
    let large = mirrored(&numbered(200), Mode::Personal);
    assert!(matches!(
        check_safety(&plan(&numbered(99), &large, Mode::Personal), &limited),
        Err(SafetyViolation::MassDelete { deleting: 101, .. })
    ));

    // Half is allowed, and so is anything at or below the floor.
    assert_eq!(
        check_safety(&plan(&numbered(20), &dest, Mode::Personal), &limited),
        Ok(())
    );
    let small = mirrored(&numbered(12), Mode::Personal);
    assert_eq!(
        check_safety(&plan(&numbered(2), &small, Mode::Personal), &limited),
        Ok(())
    );
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
