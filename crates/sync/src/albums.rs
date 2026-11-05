//! The PC's albums on the phone: the manifest's `albums` mirrored into the phone's own library, as ordinary albums inside one
//! top-level album folder, [`FOLDER_NAME`]. View-only: every refresh puts the folder back the way the manifest says.
//!
//! Everything goes through the phone engine's own album commands (`album.create`, `album.rename`, `album.reorder`, `album.addPhotos`,
//! `album.removePhotos`, `album.delete`) on the phone's own catalog. Albums outside the folder are never read for anything but their
//! place in the tree and are never changed. Inside the folder, whatever the manifest does not list is deleted (the photos stay in
//! the library); a mirrored album is found again by the id the state remembers, so a rename on the PC renames it here.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::Engine;
use crate::manifest::Album;

/// The album folder the PC's albums live in.
pub const FOLDER_NAME: &str = "From PC";

/// What the state remembers between refreshes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Memory {
    /// The phone's id of the "From PC" folder.
    pub folder: Option<u64>,
    /// The manifest's album id -> the phone's album id.
    pub albums: std::collections::BTreeMap<String, u64>,
}

/// What a mirror pass did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub created: usize,
    pub renamed: usize,
    pub deleted: usize,
    pub photos_added: usize,
    pub photos_removed: usize,
}

impl Summary {
    pub fn changed(&self) -> bool {
        *self != Summary::default()
    }
}

#[derive(Debug, Clone)]
struct Node {
    name: String,
    folder: bool,
    parent: Option<u64>,
    children: Vec<u64>,
}

#[derive(Debug, Default)]
struct Tree {
    nodes: HashMap<u64, Node>,
    top: Vec<u64>,
}

impl Tree {
    fn read(engine: &mut dyn Engine) -> Result<Tree, String> {
        let v = engine.call("albums.list", json!({}))?;
        let mut t = Tree::default();
        fn walk(t: &mut Tree, list: &[Value], parent: Option<u64>, depth: usize) {
            if depth > 64 {
                return;
            }
            for a in list {
                let Some(id) = a.get("id").and_then(Value::as_u64) else { continue };
                let kids = a.get("children").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
                t.nodes.insert(
                    id,
                    Node {
                        name: a.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                        folder: a.get("folder").and_then(Value::as_bool).unwrap_or(false),
                        parent,
                        children: kids.iter().filter_map(|k| k.get("id").and_then(Value::as_u64)).collect(),
                    },
                );
                match parent {
                    Some(p) => {
                        if let Some(n) = t.nodes.get_mut(&p)
                            && !n.children.contains(&id) {
                                n.children.push(id);
                            }
                    }
                    None => t.top.push(id),
                }
                walk(t, kids, Some(id), depth + 1);
            }
        }
        walk(&mut t, v.as_array().map(Vec::as_slice).unwrap_or_default(), None, 0);
        Ok(t)
    }

    fn add(&mut self, id: u64, name: &str, folder: bool, parent: Option<u64>) {
        self.nodes.insert(id, Node { name: name.to_string(), folder, parent, children: vec![] });
        match parent.and_then(|p| self.nodes.get_mut(&p)) {
            Some(p) => p.children.push(id),
            None => self.top.push(id),
        }
    }

    fn children(&self, parent: Option<u64>) -> Vec<u64> {
        match parent {
            Some(p) => self.nodes.get(&p).map(|n| n.children.clone()).unwrap_or_default(),
            None => self.top.clone(),
        }
    }

    /// Whether `id` is `root` or inside it.
    fn within(&self, mut id: u64, root: u64) -> bool {
        for _ in 0..=64 {
            if id == root {
                return true;
            }
            match self.nodes.get(&id).and_then(|n| n.parent) {
                Some(p) => id = p,
                None => return false,
            }
        }
        false
    }
}

fn new_id(r: &Value, what: &str) -> Result<u64, String> {
    r.get("id").and_then(Value::as_u64).ok_or_else(|| format!("{what}: the engine gave no album id"))
}

/// The members of a phone album (engine photo ids), by `catalog.query` with the album filter.
fn members(engine: &mut dyn Engine, album: u64) -> Result<Vec<u64>, String> {
    let mut out = Vec::new();
    let page = 5000u64;
    loop {
        let r = engine.call("catalog.query", json!({"filter": {"album": album}, "offset": out.len(), "limit": page}))?;
        let ids: Vec<u64> = r.get("photos").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.get("id").and_then(Value::as_u64)).collect();
        let total = r.get("total").and_then(Value::as_u64).unwrap_or(0);
        let n = ids.len();
        out.extend(ids);
        if n == 0 || (out.len() as u64) >= total {
            return Ok(out);
        }
    }
}

/// Mirror `wanted` into the phone's library. `engine_of`: the phone's photo id for a manifest photo id (a photo the phone has no record
/// of is left out). `mem` is updated as the pass goes, also when it fails half way (the next pass finishes the job).
pub fn mirror(engine: &mut dyn Engine, wanted: &[Album], engine_of: &dyn Fn(&str) -> Option<u64>, mem: &mut Memory) -> Result<Summary, String> {
    let mut sel = Selection::default();
    let r = mirror_inner(engine, wanted, engine_of, mem, &mut sel);
    sel.restore(engine);
    r
}

/// The photo selection of the phone's library. `album.addPhotos` and `album.removePhotos` are only enabled while something is selected,
/// even when they are given the photos: so, if the engine says so, the first photo is selected for the call, and the selection the
/// user had is put back when the pass is over.
#[derive(Default)]
struct Selection {
    /// The selection before this pass touched it: `{ids, active}`.
    saved: Option<Value>,
}

impl Selection {
    fn call(&mut self, engine: &mut dyn Engine, command: &str, params: Value, pick: u64) -> Result<Value, String> {
        match engine.call(command, params.clone()) {
            Err(e) if e.contains("no photos selected") => {
                if self.saved.is_none() {
                    let state = engine.call("library.state", json!({}))?;
                    self.saved = Some(state.get("selection").cloned().unwrap_or(Value::Null));
                }
                engine.call("library.select", json!({"ids": [pick]}))?;
                engine.call(command, params)
            }
            r => r,
        }
    }

    fn restore(&mut self, engine: &mut dyn Engine) {
        let Some(saved) = self.saved.take() else { return };
        let ids: Vec<u64> = saved.get("ids").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64).collect();
        let mut p = json!({"ids": ids});
        if let Some(a) = saved.get("active").and_then(Value::as_u64) {
            p["active"] = json!(a);
        }
        // (a photo that is gone since makes the engine refuse; then the selection is simply left as it is)
        let _ = engine.call("library.select", p);
    }
}

fn mirror_inner(engine: &mut dyn Engine, wanted: &[Album], engine_of: &dyn Fn(&str) -> Option<u64>, mem: &mut Memory, sel: &mut Selection) -> Result<Summary, String> {
    let mut sum = Summary::default();
    let mut tree = Tree::read(engine)?;
    // 1. the folder
    let root = mem.folder.filter(|f| tree.nodes.get(f).is_some_and(|n| n.folder && n.parent.is_none())).or_else(|| {
        // (a state that was lost: the folder with the name is the mirror's)
        tree.top.iter().copied().find(|id| tree.nodes.get(id).is_some_and(|n| n.folder && n.name == FOLDER_NAME))
    });
    if wanted.is_empty() {
        if let Some(root) = root {
            engine.call("album.delete", json!({"id": root}))?;
            sum.deleted += 1;
        }
        mem.folder = None;
        mem.albums.clear();
        return Ok(sum);
    }
    let root = match root {
        Some(r) => r,
        None => {
            let id = new_id(&engine.call("album.create", json!({"name": FOLDER_NAME, "folder": true}))?, "album.create")?;
            tree.add(id, FOLDER_NAME, true, None);
            sum.created += 1;
            id
        }
    };
    mem.folder = Some(root);
    if tree.nodes.get(&root).is_some_and(|n| n.name != FOLDER_NAME) {
        engine.call("album.rename", json!({"id": root, "name": FOLDER_NAME}))?;
        sum.renamed += 1;
    }
    // 2. which phone album each wanted album is: the one remembered, if it is still an album inside the folder
    let mut claimed: HashSet<u64> = HashSet::new();
    let mut found: HashMap<&str, u64> = HashMap::new();
    for w in wanted {
        if let Some(id) = mem.albums.get(&w.id).copied()
            && tree.nodes.get(&id).is_some_and(|n| !n.folder)
            && tree.within(id, root)
            && claimed.insert(id)
        {
            found.insert(&w.id, id);
        }
    }
    // 3. places: the folders of each path (made as needed), the albums not found adopted by name or made
    let mut by_parent: Vec<(u64, Vec<u64>)> = Vec::new();
    let mut pid_of: HashMap<&str, u64> = HashMap::new();
    for w in wanted {
        let mut parent = root;
        for name in &w.path {
            let existing = tree.children(Some(parent)).into_iter().find(|c| tree.nodes.get(c).is_some_and(|n| n.folder && n.name == *name));
            parent = match existing {
                Some(f) => f,
                None => {
                    let id = new_id(&engine.call("album.create", json!({"name": name, "parent": parent, "folder": true}))?, "album.create")?;
                    tree.add(id, name, true, Some(parent));
                    sum.created += 1;
                    id
                }
            };
        }
        let pid = match found.get(w.id.as_str()).copied() {
            Some(id) => id,
            None => {
                let adopt = tree.children(Some(parent)).into_iter().find(|c| !claimed.contains(c) && tree.nodes.get(c).is_some_and(|n| !n.folder && n.name == w.name));
                match adopt {
                    Some(id) => {
                        claimed.insert(id);
                        id
                    }
                    None => {
                        let id = new_id(&engine.call("album.create", json!({"name": w.name, "parent": parent}))?, "album.create")?;
                        tree.add(id, &w.name, false, Some(parent));
                        claimed.insert(id);
                        sum.created += 1;
                        id
                    }
                }
            }
        };
        mem.albums.insert(w.id.clone(), pid);
        pid_of.insert(&w.id, pid);
        let node = tree.nodes.get(&pid).cloned();
        if let Some(n) = &node
            && n.name != w.name
        {
            engine.call("album.rename", json!({"id": pid, "name": w.name}))?;
            if let Some(n) = tree.nodes.get_mut(&pid) {
                n.name = w.name.clone();
            }
            sum.renamed += 1;
        }
        if node.as_ref().is_some_and(|n| n.parent != Some(parent)) {
            engine.call("album.move", json!({"id": pid, "parent": parent}))?;
            if let Some(old) = node.and_then(|n| n.parent).and_then(|p| tree.nodes.get_mut(&p)) {
                old.children.retain(|c| *c != pid);
            }
            if let Some(n) = tree.nodes.get_mut(&pid) {
                n.parent = Some(parent);
            }
            if let Some(p) = tree.nodes.get_mut(&parent) {
                p.children.push(pid);
            }
        }
        match by_parent.iter_mut().find(|(p, _)| *p == parent) {
            Some((_, v)) => v.push(pid),
            None => by_parent.push((parent, vec![pid])),
        }
    }
    // (the albums of the manifest no longer in it)
    let wanted_ids: HashSet<&str> = wanted.iter().map(|w| w.id.as_str()).collect();
    mem.albums.retain(|k, _| wanted_ids.contains(k.as_str()));
    // 4. order: the manifest's, in each folder (the tree lists by id, so the order is simply set again; an album already in place costs nothing)
    for (parent, ids) in &by_parent {
        if ids.len() > 1 {
            for id in ids {
                engine.call("album.reorder", json!({"id": id, "parent": parent}))?;
            }
        }
    }
    // 5. members
    for w in wanted {
        let pid = pid_of[w.id.as_str()];
        let mut want: Vec<u64> = Vec::new();
        let mut seen = HashSet::new();
        for id in w.photos.iter().filter_map(|p| engine_of(p)) {
            if seen.insert(id) {
                want.push(id);
            }
        }
        let have = members(engine, pid)?;
        let have_set: HashSet<u64> = have.iter().copied().collect();
        let add: Vec<u64> = want.iter().copied().filter(|i| !have_set.contains(i)).collect();
        let remove: Vec<u64> = have.iter().copied().filter(|i| !seen.contains(i)).collect();
        if !add.is_empty() {
            sel.call(engine, "album.addPhotos", json!({"id": pid, "ids": add}), add[0])?;
            sum.photos_added += add.len();
        }
        if !remove.is_empty() {
            sel.call(engine, "album.removePhotos", json!({"id": pid, "ids": remove}), remove[0])?;
            sum.photos_removed += remove.len();
        }
    }
    // 6. what the manifest does not list, inside the folder, goes (an album, or a folder with no listed album in it)
    let keep: HashSet<u64> = pid_of.values().copied().collect();
    fn has_kept(tree: &Tree, id: u64, keep: &HashSet<u64>) -> bool {
        tree.nodes.get(&id).is_some_and(|n| if n.folder { n.children.iter().any(|c| has_kept(tree, *c, keep)) } else { keep.contains(&id) })
    }
    // (a folder with nothing listed in it goes whole; one with something keeps its own listed albums and loses the rest)
    fn prune(tree: &Tree, folder: u64, keep: &HashSet<u64>, doomed: &mut Vec<u64>) {
        for c in tree.children(Some(folder)) {
            let Some(n) = tree.nodes.get(&c) else { continue };
            if !has_kept(tree, c, keep) {
                doomed.push(c);
            } else if n.folder {
                prune(tree, c, keep, doomed);
            }
        }
    }
    let mut doomed = Vec::new();
    prune(&tree, root, &keep, &mut doomed);
    for id in doomed {
        engine.call("album.delete", json!({"id": id}))?;
        sum.deleted += 1;
    }
    Ok(sum)
}
