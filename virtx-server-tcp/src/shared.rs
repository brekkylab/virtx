//! What the binary shares its projects' anonymous pulls through, where its config says to
//! (`share_public_images`): [`SharedPulls`].

use futures_util::future::BoxFuture;
use virtx_uvm::{Digest, ImageStore, ProjectStore, PublicImages};

/// The images this server's projects pulled anonymously, shared between them: a pull in one
/// project of an image another pulled anonymously before files that one's, its layers linked,
/// and pulls none of it. Each project's store keeps the origins of its pulls, so this keeps
/// nothing of its own: it looks in the others' as a pull asks.
///
/// How long a pull takes then says whether another project pulled that image before, so a
/// server shares so only where its projects' users may know that of each other.
#[derive(Clone)]
pub struct SharedPulls {
    projects: ProjectStore,
}

impl SharedPulls {
    /// Sharing between the projects kept in `projects`.
    pub fn new(projects: ProjectStore) -> SharedPulls {
        SharedPulls { projects }
    }
}

impl PublicImages for SharedPulls {
    fn fetch<'a>(
        &'a self,
        store: &'a ImageStore,
        origin: &'a Digest,
    ) -> BoxFuture<'a, anyhow::Result<Option<Digest>>> {
        let (projects, store, origin) = (self.projects.clone(), store.clone(), origin.clone());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Digest>> {
                for id in projects.list()? {
                    let Some(from) = ImageStore::open(projects.path(&id).join("images")) else {
                        continue;
                    };
                    if from.root() == store.root() {
                        continue;
                    }
                    if let Some(image) = store.share_from(&from, &origin)? {
                        return Ok(Some(image));
                    }
                }
                Ok(None)
            })
            .await?
        })
    }

    /// Nothing to keep: the project that pulled it keeps its origin, where [`fetch`] finds it.
    ///
    /// [`fetch`]: PublicImages::fetch
    fn pulled(&self, _: &Digest, _: &[u8], _: &[u8], _: &Digest) {}
}

#[cfg(test)]
mod tests {
    use virtx_uvm::{FsTree, ImageConfiguration, ProjectId};

    use super::*;

    /// A pull in one project of an image another pulled anonymously files that one's, its
    /// layers one file with theirs; one no other project pulled, one pulled with credentials or
    /// pushed, or the asking project's own, is not shared.
    #[tokio::test]
    async fn a_project_s_pull_shares_what_another_pulled() {
        let home = tempfile::tempdir().unwrap();
        let projects = ProjectStore::new(home.path());
        let (pulled, pulling) = (ProjectId::new(), ProjectId::new());
        let store = |id: &ProjectId| ImageStore::new(projects.path(id).join("images")).unwrap();
        let (from, to) = (store(&pulled), store(&pulling));

        let layer = from.insert_layer(&FsTree::new()).unwrap();
        let image = from
            .insert_image(&ImageConfiguration::default(), std::slice::from_ref(&layer))
            .unwrap();
        let origin: Digest = format!("sha256:{}", "a".repeat(64)).parse().unwrap();
        from.insert_anonymous_image_origin(&origin, b"served", b"its config", &image)
            .unwrap();

        let shared = SharedPulls::new(projects);
        assert_eq!(
            shared.fetch(&to, &origin).await.unwrap(),
            Some(image.clone())
        );
        assert_eq!(to.get_image_origin(&origin).unwrap(), Some(image.clone()));
        assert!(to.get_image(&image).unwrap().is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let inode = |store: &ImageStore| std::fs::metadata(store.layer_path(&layer)).unwrap();
            assert_eq!(inode(&from).ino(), inode(&to).ino(), "one file");
        }

        let other: Digest = format!("sha256:{}", "b".repeat(64)).parse().unwrap();
        assert_eq!(shared.fetch(&to, &other).await.unwrap(), None);

        // One pulled with credentials, or pushed, is not shared, whoever asks.
        let private: Digest = format!("sha256:{}", "c".repeat(64)).parse().unwrap();
        from.insert_image_origin(&private, b"served", b"its config", &image)
            .unwrap();
        assert_eq!(shared.fetch(&to, &private).await.unwrap(), None);
        assert_eq!(to.get_image_origin(&private).unwrap(), None);
        let own = ImageStore::new(home.path().join("elsewhere")).unwrap();
        let alone = SharedPulls::new(ProjectStore::new(home.path().join("none")));
        assert_eq!(alone.fetch(&own, &origin).await.unwrap(), None);
    }
}
