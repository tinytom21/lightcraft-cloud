//! What the other PC remembers between passes: `<state>/other-state.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::write_atomic;
use crate::{Error, Result};

/// One file the cloud library has (a photo, or the partner of one), by its content hash.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct Rec {
    /// Where it is, relative to the photo folder (`/`-separated).
    pub path: String,
    /// The local sidecar as of the last pass (relative to the photo folder), for the status and for moving it aside.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<String>,
    /// The manifest's `sidecarVersion` this PC has reconciled with (applied, found equal, or sent over).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// The version a new edit starts from (the inbox file's `base`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// SHA-256 of the local sidecar bytes the cloud library has (downloaded, or sent): a local sidecar with other bytes is an edit not
    /// sent yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synced: Option<String>,
    /// After an edit was sent: the manifest's version (`none` if it had no sidecar) at that moment. While the manifest still says that,
    /// the home PC has not taken the edit, and the local sidecar is left alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub awaiting: Option<String>,
    /// The stamp of the last inbox file for this photo (a new one is always later).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stamp: Option<String>,
    /// Deleted on this PC: not downloaded again (see the module docs).
    #[serde(skip_serializing_if = "is_false")]
    pub gone: bool,
    /// A photo this PC imported itself: matched by its id with what the home PC published, wherever the home PC put it (the file here
    /// stays where it is, whatever it is called there).
    #[serde(skip_serializing_if = "is_false")]
    pub imported: bool,
    /// Being placed in the photo folder (or LightCraft not told yet): a crash or a "no" from the hook leaves it so; the next pass finishes.
    #[serde(skip_serializing_if = "is_false")]
    pub placing: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct State {
    pub format: u32,
    /// The device name this PC has used in the cloud library (a name not claimed yet is checked against `inbox/`).
    pub device: Option<String>,
    /// By content hash.
    pub photos: BTreeMap<String, Rec>,
    /// Photos of the chosen folders not here after the last pass.
    pub waiting: usize,
    pub last_pass: Option<String>,
    #[serde(skip)]
    pub path: PathBuf,
}

impl State {
    pub fn open(dir: &Path) -> Result<State> {
        let path = dir.join("other-state.json");
        let mut st = match std::fs::read(&path) {
            Ok(b) => match serde_json::from_slice::<State>(&b) {
                Ok(s) => s,
                Err(e) => {
                    // (what this PC had received is then found again by its bytes, and what it had sent again by the sidecars' contents)
                    log::warn!("other-state.json unreadable ({e}): starting over");
                    let _ = std::fs::rename(&path, dir.join("other-state.json.corrupt"));
                    State::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(Error::io(path.display(), e)),
        };
        st.format = 1;
        st.path = path;
        Ok(st)
    }

    pub fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec(self).map_err(|e| Error::Io(format!("other-state: {e}")))?;
        write_atomic(&self.path, &bytes)
    }
}
