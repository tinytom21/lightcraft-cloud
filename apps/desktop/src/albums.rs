//! The albums of the running LightCraft, for the manifest: an [`AlbumSource`] over the control channel's link.
//!
//! Only the three read-only queries the gate admits ([`crate::lightcraft::allowed`]) are made: `albums.list`, `catalog.query` with the
//! album filter, `photo.inspect`. Nothing is written to LightCraft and no library is opened.

use lightcraft_cloud_client::home::{AlbumNode, AlbumSource, MemberPage, SourceError};

use crate::lightcraft::{LightCraftLink, LinkError};

/// Reads albums through a link to LightCraft, connecting (and checking that what answers is LightCraft) when it has to.
pub struct LinkAlbums {
    link: Box<dyn LightCraftLink>,
    ready: bool,
}

impl LinkAlbums {
    pub fn new(link: Box<dyn LightCraftLink>) -> LinkAlbums {
        LinkAlbums { link, ready: false }
    }

    fn with<T>(&mut self, f: impl FnOnce(&mut dyn LightCraftLink) -> Result<T, LinkError>) -> Result<T, SourceError> {
        if !self.ready {
            self.link.identify().map_err(error)?;
            self.ready = true;
        }
        f(&mut *self.link).map_err(|e| {
            // (any error drops the connection)
            self.ready = false;
            error(e)
        })
    }
}

fn error(e: LinkError) -> SourceError {
    if e.is_unreachable() { SourceError::Unreachable(e.text().to_string()) } else { SourceError::Failed(e.text().to_string()) }
}

impl AlbumSource for LinkAlbums {
    fn albums(&mut self) -> Result<Vec<AlbumNode>, SourceError> {
        self.with(|l| l.albums_list())
    }

    fn members(&mut self, album: u64, offset: u64, limit: u64) -> Result<MemberPage, SourceError> {
        self.with(|l| l.album_members(album, offset, limit))
    }

    fn content_hash(&mut self, photo: u64) -> Result<Option<String>, SourceError> {
        self.with(|l| l.photo_hash(photo))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_cloud_client::home::read_albums;
    use std::collections::HashMap;

    struct Fake {
        calls: Vec<String>,
        open: bool,
    }

    impl LightCraftLink for Fake {
        fn identify(&mut self) -> Result<(), LinkError> {
            self.calls.push("identify".into());
            if self.open { Ok(()) } else { Err(LinkError::Unreachable("connection refused".into())) }
        }
        fn import_add(&mut self, _: &[String]) -> Result<crate::lightcraft::ImportReply, LinkError> {
            panic!("the albums never import")
        }
        fn albums_list(&mut self) -> Result<Vec<AlbumNode>, LinkError> {
            self.calls.push("albums.list".into());
            Ok(vec![AlbumNode { id: 1, name: "A".into(), folder: false, smart: false, children: vec![] }])
        }
        fn album_members(&mut self, album: u64, offset: u64, limit: u64) -> Result<MemberPage, LinkError> {
            self.calls.push(format!("members {album} {offset} {limit}"));
            Ok(MemberPage { total: 2, ids: vec![5, 6] })
        }
        fn photo_hash(&mut self, id: u64) -> Result<Option<String>, LinkError> {
            self.calls.push(format!("inspect {id}"));
            Ok((id == 5).then(|| "5".repeat(32)))
        }
    }

    #[test]
    fn it_connects_once_and_reads_the_albums_with_the_three_queries() {
        let mut src = LinkAlbums::new(Box::new(Fake { calls: vec![], open: true }));
        let r = read_albums(&mut src, &mut HashMap::new(), 100).unwrap();
        assert_eq!(r.albums.len(), 1);
        assert_eq!(r.albums[0].hashes, vec!["5".repeat(32)], "a photo with no hash is left out");
        assert!(r.complete);
    }

    #[test]
    fn a_closed_lightcraft_is_unreachable_and_reads_nothing() {
        let mut src = LinkAlbums::new(Box::new(Fake { calls: vec![], open: false }));
        assert!(matches!(read_albums(&mut src, &mut HashMap::new(), 100), Err(SourceError::Unreachable(_))));
    }
}
