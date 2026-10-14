use std::collections::{BTreeMap, HashMap};

use serde_json::json;

use super::*;
use crate::vault::Attachment;

fn text(value: &str) -> Node {
    Node::secret(value)
}

fn map(entries: Vec<(&str, Node)>) -> Node {
    Node::Map(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
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
            map(vec![("username", text("user")), ("password", text(password)), ("totp", Node::Null)]),
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
    let plan = plan(source, dest, mode);
    let mut next = dest.clone();
    let mut counter = dest.items.len() + dest.folders.len() + dest.collections.len();
    let mut fresh = |prefix: &str| {
        counter += 1;
        format!("{prefix}-{counter}")
    };

    for name in &plan.folders_to_create {
        let id = fresh("dest-folder");
        next.folders.push(folder(&id, name));
    }
    for new in &plan.collections_to_create {
        let id = fresh("dest-collection");
        next.collections.push(collection(&id, &new.name));
    }
    let doomed_folders: Vec<&str> = plan.folders_to_delete.iter().map(|f| f.id.as_str()).collect();
    let doomed_collections: Vec<&str> = plan.collections_to_delete.iter().map(|c| c.id.as_str()).collect();
    let folder_ids: HashMap<String, String> = next
        .folders
        .iter()
        .filter(|f| !doomed_folders.contains(&f.id.as_str()))
        .map(|f| (f.name.clone(), f.id.clone()))
        .collect();
    let collection_ids: HashMap<String, String> = next
        .collections
        .iter()
        .filter(|c| !doomed_collections.contains(&c.id.as_str()))
        .map(|c| (c.name.clone(), c.id.clone()))
        .collect();

    let write = |item: &mut Item, entry: &Entry| {
        let draft = plan.draft(entry, source, &folder_ids, &collection_ids).unwrap();
        item.content = draft.content;
        item.folder_id = draft.folder_id;
        item.collection_ids = draft.collection_ids;
    };
    for &entry_index in &plan.creates {
        let entry = &plan.entries[entry_index];
        let source_item = &source.items[entry.source];
        let mut item = login(&fresh("dest-item"), "", "");
        write(&mut item, entry);
        item.archived = source_item.archived;
        item.attachments = source_item
            .attachments
            .iter()
            .map(|a| attachment(&fresh("dest-attachment"), &a.file_name, a.size))
            .collect();
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
        }
        if let Some(archived) = update.set_archived {
            item.archived = archived;
        }
    }
    let doomed: Vec<String> = plan.deletes.iter().map(|d| dest.items[d.dest].id.clone()).collect();
    next.items.retain(|item| !doomed.contains(&item.id));
    next.folders.retain(|f| !doomed_folders.contains(&f.id.as_str()));
    next.collections.retain(|c| !doomed_collections.contains(&c.id.as_str()));
    next
}

fn assert_converged(source: &Vault, dest: &Vault, mode: Mode) {
    let again = plan(source, dest, mode);
    assert!(!again.has_changes(), "plan still has changes: {again:#?}");
    let live = source.items.iter().filter(|item| !item.in_trash).count();
    assert_eq!(again.unchanged, live);
    assert_eq!(dest.items.len(), live);
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
    source.items[0]
        .content
        .insert("fields".into(), Node::List(vec![map(vec![("name", text("pin")), ("value", text("1234"))])]));
    let dest = mirrored(&source, Mode::Personal);

    let fields = dest.items[0].fields();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].as_map().unwrap()["name"], text("pin"));
    assert!(is_marker_field(&fields[1]));
    assert_eq!(fields[1].as_map().unwrap()["type"], Node::Number(1.into()));
    let marker = marker_of(&dest.items[0]).unwrap();
    assert_eq!(marker.source_id, "s1");
    assert_eq!(marker.attachments_hash.as_deref(), Some("-"));
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
        [Update { entry: 1, dest: 1, write_content: true, sync_attachments: false, set_archived: None }]
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
    let Some(Node::Map(login)) = dest.items[0].content.get_mut("login") else { panic!() };
    login.insert("password".into(), text("tampered"));

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.updates.len(), 1);
    assert!(plan.updates[0].write_content);
    assert_converged(&source, &apply(&source, &dest, Mode::Personal), Mode::Personal);
}

#[test]
fn destination_items_without_a_live_source_are_deleted_with_a_reason() {
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
    let reasons: Vec<(&str, DeleteReason)> =
        plan.deletes.iter().map(|d| (dest.items[d.dest].id.as_str(), d.reason)).collect();
    assert_eq!(
        reasons,
        [
            ("dest-item-2", DeleteReason::Stale),
            ("manual", DeleteReason::Untracked),
            ("in-trash", DeleteReason::Trashed),
            ("copy", DeleteReason::Duplicate),
        ]
    );
    assert_eq!(plan.unchanged, 1);
    assert_converged(&source, &apply(&source, &dest, Mode::Personal), Mode::Personal);
}

#[test]
fn a_malformed_marker_is_not_trusted() {
    let source = vault(vec![login("s1", "One", "pw1")]);
    let mut dest = mirrored(&source, Mode::Personal);
    let Some(Node::List(fields)) = dest.items[0].content.get_mut("fields") else { panic!() };
    fields[0] = map(vec![("name", text(MARKER_FIELD)), ("value", text("only-one-part"))]);

    assert_eq!(marker_of(&dest.items[0]), None);
    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.creates, [0]);
    assert_eq!(plan.deletes, [Delete { dest: 0, reason: DeleteReason::Untracked }]);
}

#[test]
fn items_in_the_source_trash_are_not_mirrored() {
    let mut source = vault(vec![login("s1", "One", "pw1"), login("s2", "Two", "pw2")]);
    let dest = mirrored(&source, Mode::Personal);
    source.items[1].in_trash = true;

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(plan.deletes, [Delete { dest: 1, reason: DeleteReason::Stale }]);
    assert_converged(&source, &apply(&source, &dest, Mode::Personal), Mode::Personal);
}

#[test]
fn attachments_are_copied_again_only_when_they_changed() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.items[0].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let dest = mirrored(&source, Mode::Personal);
    assert_eq!(plan(&source, &Vault::default(), Mode::Personal).summary(&source).attachments_copied, 1);

    // A content change leaves the attachment alone.
    let mut edited = source.clone();
    edited.items[0].content.insert("name".into(), text("Renamed"));
    let update = &plan(&edited, &dest, Mode::Personal).updates[0];
    assert!(update.write_content && !update.sync_attachments);

    // A file replaced under the same name and size gets a new attachment id.
    let mut replaced = source.clone();
    replaced.items[0].attachments = vec![attachment("a2", "scan.pdf", 100)];
    let update = &plan(&replaced, &dest, Mode::Personal).updates[0];
    assert!(update.sync_attachments);
    assert!(update.write_content, "the marker has to record the new attachment");
    assert_converged(&replaced, &apply(&replaced, &dest, Mode::Personal), Mode::Personal);

    // An attachment removed from the destination by hand is restored.
    let mut damaged = dest.clone();
    damaged.items[0].attachments.clear();
    let update = &plan(&source, &damaged, Mode::Personal).updates[0];
    assert!(update.sync_attachments && !update.write_content);

    // Removing the last source attachment removes it from the destination.
    let mut removed = source.clone();
    removed.items[0].attachments.clear();
    let after = apply(&removed, &dest, Mode::Personal);
    assert!(after.items[0].attachments.is_empty());
    assert_converged(&removed, &after, Mode::Personal);
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
fn markers_from_the_python_tool_are_adopted_in_place() {
    let mut source = vault(vec![login("s1", "Plain", "pw1"), login("s2", "With file", "pw2")]);
    source.items[1].attachments = vec![attachment("a1", "scan.pdf", 100)];
    let mut dest = mirrored(&source, Mode::Personal);
    for item in &mut dest.items {
        let Some(Node::List(fields)) = item.content.get_mut("fields") else { panic!() };
        let source_id = marker_of_fields(fields);
        *fields.last_mut().unwrap() =
            map(vec![("name", text(MARKER_FIELD)), ("value", text(&format!("{source_id} 0123abcd")))]);
    }

    let plan = plan(&source, &dest, Mode::Personal);
    assert!(plan.creates.is_empty() && plan.deletes.is_empty());
    assert_eq!(plan.updates.len(), 2);
    assert!(plan.updates.iter().all(|u| u.write_content));
    // Without attachments there is nothing to re-copy; with them the old
    // marker cannot vouch for the files.
    assert!(!plan.updates[0].sync_attachments);
    assert!(plan.updates[1].sync_attachments);
    assert_converged(&source, &apply(&source, &dest, Mode::Personal), Mode::Personal);
}

fn marker_of_fields(fields: &[Node]) -> String {
    let value = fields.last().unwrap().as_map().unwrap()["value"].as_str().unwrap();
    value.split_whitespace().next().unwrap().to_string()
}

#[test]
fn archive_state_is_mirrored_without_rewriting_content() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    let dest = mirrored(&source, Mode::Personal);
    source.items[0].archived = true;

    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(
        plan.updates,
        [Update { entry: 0, dest: 0, write_content: false, sync_attachments: false, set_archived: Some(true) }]
    );
    assert_converged(&source, &apply(&source, &dest, Mode::Personal), Mode::Personal);
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
    assert_eq!(after.items[0].folder_id.as_deref(), Some(after.folders[0].id.as_str()));
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
    assert_converged(&source, &apply(&source, &dest, Mode::Personal), Mode::Personal);
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
    source.collections = vec![collection("c1", "Team A"), collection("c2", "Team B"), collection("c3", "Empty")];
    source.collections[1].external_id = Some("ext-b".into());
    source.items[0].collection_ids = vec!["c2".into(), "c1".into()];
    source.items[1].collection_ids = vec!["c1".into()];
    // Folders are a per-user view and are not part of an organization mirror.
    source.folders = vec![folder("f1", "Mine")];
    source.items[0].folder_id = Some("f1".into());

    let first = plan(&source, &Vault::default(), Mode::Organization);
    assert!(first.folders_to_create.is_empty());
    let names: Vec<&str> = first.collections_to_create.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Empty", "Team A", "Team B"]);
    assert_eq!(first.collections_to_create[2].external_id.as_deref(), Some("ext-b"));

    let dest = mirrored(&source, Mode::Organization);
    assert_eq!(dest.items[0].folder_id, None);
    assert_eq!(dest.items[0].collection_ids.len(), 2);
    assert!(dest.items[0].collection_ids.iter().all(|id| id.starts_with("dest-collection")));

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
    assert_converged(&moved, &apply(&moved, &dest, Mode::Organization), Mode::Organization);
}

#[test]
fn server_echo_differences_are_not_changes() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    let Some(Node::Map(source_login)) = source.items[0].content.get_mut("login") else { panic!() };
    source_login.insert(
        "uris".into(),
        Node::List(vec![map(vec![("uri", text("https://example.com")), ("match", Node::Null)])]),
    );
    source_login.insert("passwordRevisionDate".into(), Node::Text("2026-03-04T05:06:07.1234567Z".into()));
    let mut dest = mirrored(&source, Mode::Personal);

    let content = &mut dest.items[0].content;
    // Fields that are null on one server are missing or empty on another.
    content.remove("notes");
    content.insert("passwordHistory".into(), Node::List(vec![]));
    content.insert("card".into(), Node::Null);
    content.insert("secureNote".into(), map(vec![("type", Node::Null)]));
    let Some(Node::Map(dest_login)) = content.get_mut("login") else { panic!() };
    dest_login.remove("totp");
    dest_login.insert("fido2Credentials".into(), Node::Null);
    // The first URI is echoed for old clients.
    dest_login.insert("uri".into(), text("https://example.com"));
    // Timestamps come back in another precision.
    dest_login.insert("passwordRevisionDate".into(), Node::Text("2026-03-04T05:06:07.123Z".into()));
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
    source.items[0].content.insert("notes".into(), text("now with text"));
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
        map(vec![("number", text("X1234567")), ("issued", Node::Text("2026-01-01T00:00:00Z".into()))]),
    );
    let dest = mirrored(&source, Mode::Personal);
    assert_eq!(dest.items[0].content["passport"].as_map().unwrap()["number"], text("X1234567"));

    let Some(Node::Map(passport)) = source.items[0].content.get_mut("passport") else { panic!() };
    passport.insert("number".into(), text("Y7654321"));
    assert_eq!(plan(&source, &dest, Mode::Personal).updates.len(), 1);
}

#[test]
fn a_draft_names_the_container_that_is_missing() {
    let mut source = vault(vec![login("s1", "One", "pw1")]);
    source.folders = vec![folder("f1", "Work")];
    source.items[0].folder_id = Some("f1".into());
    let plan = plan(&source, &Vault::default(), Mode::Personal);
    let error = plan.draft(&plan.entries[0], &source, &HashMap::new(), &HashMap::new()).unwrap_err();
    assert_eq!(error, PlanError::MissingFolder("Work".into()));
}

fn numbered(count: usize) -> Vault {
    vault((0..count).map(|i| login(&format!("s{i}"), &format!("Item {i}"), "pw")).collect())
}

#[test]
fn a_source_that_carries_markers_is_refused() {
    let original = numbered(3);
    let mirror = mirrored(&original, Mode::Personal);
    // Source and destination swapped: the mirror is now the source.
    let plan = plan(&mirror, &original, Mode::Personal);
    assert_eq!(check_safety(&plan, &Safety::default()), Err(SafetyViolation::SourceLooksLikeMirror(3)));
    let lenient = Safety { allow_initial_overwrite: true, allow_mass_delete: true, ..Safety::default() };
    assert!(check_safety(&plan, &lenient).is_err(), "no flag overrides a swapped configuration");
}

#[test]
fn a_first_run_into_a_vault_with_content_needs_confirmation() {
    let source = numbered(3);
    let dest = vault(vec![login("existing", "Already here", "pw")]);
    let plan = plan(&source, &dest, Mode::Personal);
    assert_eq!(check_safety(&plan, &Safety::default()), Err(SafetyViolation::InitialOverwrite(1)));
    let confirmed = Safety { allow_initial_overwrite: true, ..Safety::default() };
    assert_eq!(check_safety(&plan, &confirmed), Ok(()));

    // An empty destination, or one that only has trash, needs no confirmation.
    assert_eq!(check_safety(&super::plan(&source, &Vault::default(), Mode::Personal), &Safety::default()), Ok(()));
    let mut trash_only = dest.clone();
    trash_only.items[0].in_trash = true;
    assert_eq!(check_safety(&super::plan(&source, &trash_only, Mode::Personal), &Safety::default()), Ok(()));
}

#[test]
fn a_stray_item_in_an_established_mirror_is_removed_without_confirmation() {
    let source = numbered(3);
    let mut dest = mirrored(&source, Mode::Personal);
    dest.items.push(login("manual", "Added by hand", "x"));
    assert_eq!(check_safety(&plan(&source, &dest, Mode::Personal), &Safety::default()), Ok(()));
}

#[test]
fn deleting_most_of_the_mirror_is_refused() {
    let dest = mirrored(&numbered(40), Mode::Personal);

    // The source suddenly looks almost empty, as after a failed or partial read.
    let shrunk = plan(&numbered(5), &dest, Mode::Personal);
    assert_eq!(
        check_safety(&shrunk, &Safety::default()),
        Err(SafetyViolation::MassDelete { deleting: 35, total: 40, percent: 87, limit: 50 })
    );
    let confirmed = Safety { allow_mass_delete: true, ..Safety::default() };
    assert_eq!(check_safety(&shrunk, &confirmed), Ok(()));

    // Half is still allowed, and so is anything at or below the floor.
    assert_eq!(check_safety(&plan(&numbered(20), &dest, Mode::Personal), &Safety::default()), Ok(()));
    let small = mirrored(&numbered(10), Mode::Personal);
    assert_eq!(check_safety(&plan(&numbered(0), &small, Mode::Personal), &Safety::default()), Ok(()));
    let eleven = mirrored(&numbered(11), Mode::Personal);
    assert!(check_safety(&plan(&numbered(0), &eleven, Mode::Personal), &Safety::default()).is_err());
}

#[test]
fn differences_are_reported_as_paths_without_values() {
    let left = json!({"content": {"name": "a", "login": {"password": "secret-1", "uris": [{"uri": "x"}]}}, "folder": null});
    let right = json!({"content": {"name": "a", "login": {"password": "secret-2", "uris": [{"uri": "y"}], "totp": "t"}}, "folder": "F"});
    let paths = differing_paths(&left, &right);
    assert_eq!(paths, ["content.login.password", "content.login.totp", "content.login.uris[0].uri", "folder"]);
    assert!(differing_paths(&left, &left).is_empty());
    assert_eq!(differing_paths(&json!([1]), &json!([1, 2])), ["<root>"]);
}
